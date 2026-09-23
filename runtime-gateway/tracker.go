package main

import (
	"context"
	"errors"
	"net"
	"net/http"
	"slices"
	"strconv"
	"sync"

	"k8s.io/apimachinery/pkg/types"
	"k8s.io/apimachinery/pkg/util/sets"
)

var ErrHostnameNotFound = errors.New("hostname not found")

type LookupResult struct {
	Hostname   string
	Scheme     string
	WorkloadID string
}

type HostResolver interface {
	Resolve(ctx context.Context, req *http.Request) LookupResult
}

// HostRegistry tracks the hosts requests can be forwarded to. Registrations are
// keyed by the Host object's key so that deregistering needs nothing but the
// key of an object that may already be gone from the API server.
type HostRegistry interface {
	RegisterHost(ctx context.Context, key types.NamespacedName, hostID string, hostname string, port int) error
	DeregisterHost(ctx context.Context, key types.NamespacedName) error
}

// WorkloadRegistry tracks which workloads serve which hostname. As with
// HostRegistry, registrations are keyed by the Workload object's key.
//
// generation identifies the rollout that produced the workload (a
// WorkloadReplicaSet's template hash — see
// runtimev1alpha1.WorkloadReplicaSetGenerationAnnotation). During a rolling
// update the previous generation's workloads stay registered, and available,
// alongside the new generation's until the operator tears them down; Resolve
// uses generation to route only to the newest one present instead of
// splitting traffic between old and new code. Workloads with no generation
// (not created by a ReplicaSet) share the empty-string generation and behave
// as before: a single pool, load-balanced across all of them.
type WorkloadRegistry interface {
	RegisterWorkload(ctx context.Context, key types.NamespacedName, hostID string, workloadID string, hostname string, generation string) error
	DeregisterWorkload(ctx context.Context, key types.NamespacedName) error
}

var _ HostResolver = (*HostTracker)(nil)
var _ HostRegistry = (*HostTracker)(nil)
var _ WorkloadRegistry = (*HostTracker)(nil)

// workloadRoute is everything a registered Workload contributes to the routing
// tables, retained so the entry can be undone from the object's key alone.
type workloadRoute struct {
	hostID     string
	workloadID string
	hostname   string
	generation string
}

// hostnameRoute is the routing state for a single hostname: the workloads
// registered for it, grouped by rollout generation, plus the order in which
// generations were first seen so Resolve can prefer the newest one.
type hostnameRoute struct {
	// generation to the set of workloadIDs currently registered for it
	generations map[string]sets.Set[string]
	// generations in the order first seen for this hostname, oldest first
	order []string
}

func (hr *hostnameRoute) addWorkload(workloadID, generation string) {
	set, ok := hr.generations[generation]
	if !ok {
		set = sets.New[string]()
		hr.generations[generation] = set
		hr.order = append(hr.order, generation)
	}
	set.Insert(workloadID)
}

func (hr *hostnameRoute) removeWorkload(workloadID, generation string) {
	set, ok := hr.generations[generation]
	if !ok {
		return
	}
	set.Delete(workloadID)
	if set.Len() == 0 {
		delete(hr.generations, generation)
		hr.order = slices.DeleteFunc(hr.order, func(g string) bool { return g == generation })
	}
}

// newestWorkload returns a workload from the newest generation that still has
// any registered, preferring it over every older generation even if the
// older one has more replicas. Older generations only remain as a fallback
// for hostnames whose newest generation has no available workloads at all.
func (hr *hostnameRoute) newestWorkload() (string, bool) {
	for i := len(hr.order) - 1; i >= 0; i-- {
		set, ok := hr.generations[hr.order[i]]
		if !ok || set.Len() == 0 {
			continue
		}
		return set.UnsortedList()[0], true
	}
	return "", false
}

func (hr *hostnameRoute) empty() bool {
	return len(hr.order) == 0
}

type HostTracker struct {
	// where to send requests that have no registered workloads
	Fallback Fallback

	lock sync.RWMutex
	// HostID to "hostname:port"
	hosts map[string]string
	// hostname to its routing state
	hostnames map[string]*hostnameRoute
	// WorkloadID to HostID
	workloads map[string]string
	// Host object key to HostID
	hostKeys map[types.NamespacedName]string
	// Workload object key to the route it registered
	workloadKeys map[types.NamespacedName]workloadRoute
}

func newHostTracker(fallback Fallback) *HostTracker {
	return &HostTracker{
		Fallback:     fallback,
		hosts:        make(map[string]string),
		hostnames:    make(map[string]*hostnameRoute),
		workloads:    make(map[string]string),
		hostKeys:     make(map[types.NamespacedName]string),
		workloadKeys: make(map[types.NamespacedName]workloadRoute),
	}
}

func (ht *HostTracker) Resolve(ctx context.Context, req *http.Request) LookupResult {
	ht.lock.RLock()
	defer ht.lock.RUnlock()

	// X-Route-Host allows WASM components to route cross-workload HTTP
	// requests despite WASI HTTP forbidding explicit Host header manipulation.
	lookupHost := req.Host
	if routeHost := req.Header.Get("X-Route-Host"); routeHost != "" {
		lookupHost = routeHost
	}

	route, ok := ht.hostnames[lookupHost]
	if !ok || route.empty() {
		scheme, endpoint := ht.Fallback.InvalidHostname(lookupHost)
		return LookupResult{
			Hostname: endpoint,
			Scheme:   scheme,
		}
	}

	// Pick a random workload from the newest generation registered for this
	// hostname, so a rollout in progress never splits traffic between old and
	// new component code.
	workloadID, ok := route.newestWorkload()
	if !ok {
		scheme, endpoint := ht.Fallback.NoWorkloads(lookupHost)
		return LookupResult{
			Hostname: endpoint,
			Scheme:   scheme,
		}
	}

	// find the host for the workload
	// (should always exist if the workload exists)
	hostID, ok := ht.workloads[workloadID]
	if !ok {
		scheme, endpoint := ht.Fallback.NoWorkloads(lookupHost)
		return LookupResult{
			Hostname: endpoint,
			Scheme:   scheme,
		}
	}

	// find the hostname:port for the host
	// (should always exist if the host is healthy)
	hostname, ok := ht.hosts[hostID]
	if !ok {
		scheme, endpoint := ht.Fallback.NoWorkloads(lookupHost)
		return LookupResult{
			Hostname: endpoint,
			Scheme:   scheme,
		}
	}

	return LookupResult{
		Hostname:   hostname,
		Scheme:     "http",
		WorkloadID: workloadID,
	}
}

func (ht *HostTracker) RegisterHost(ctx context.Context, key types.NamespacedName, hostID string, hostname string, port int) error {
	ht.lock.Lock()
	defer ht.lock.Unlock()

	// A Host object that comes back under a new ID — a host pod that restarted
	// and re-registered under the same object name — would otherwise leave its
	// previous ID routing traffic.
	if prev, ok := ht.hostKeys[key]; ok && prev != hostID {
		ht.removeHost(prev)
	}

	ht.hostKeys[key] = hostID
	ht.hosts[hostID] = net.JoinHostPort(hostname, strconv.Itoa(port))
	return nil
}

func (ht *HostTracker) DeregisterHost(ctx context.Context, key types.NamespacedName) error {
	ht.lock.Lock()
	defer ht.lock.Unlock()

	hostID, ok := ht.hostKeys[key]
	if !ok {
		return nil
	}
	delete(ht.hostKeys, key)
	ht.removeHost(hostID)
	return nil
}

// removeHost drops a host along with every workload placed on it. Leaving the
// workloads would leak memory proportional to workload churn and would cause
// stale hostname mappings if a new host ever reuses the same hostID.
//
// The caller must hold ht.lock.
func (ht *HostTracker) removeHost(hostID string) {
	for key, route := range ht.workloadKeys {
		if route.hostID == hostID {
			delete(ht.workloadKeys, key)
		}
	}
	for workloadID, hID := range ht.workloads {
		if hID != hostID {
			continue
		}
		delete(ht.workloads, workloadID)
		for hostname, route := range ht.hostnames {
			for _, generation := range route.order {
				route.removeWorkload(workloadID, generation)
			}
			if route.empty() {
				delete(ht.hostnames, hostname)
			}
		}
	}

	delete(ht.hosts, hostID)
}

func (ht *HostTracker) RegisterWorkload(ctx context.Context, key types.NamespacedName, hostID string, workloadID string, hostname string, generation string) error {
	ht.lock.Lock()
	defer ht.lock.Unlock()

	route := workloadRoute{hostID: hostID, workloadID: workloadID, hostname: hostname, generation: generation}
	// A workload that moved to another host, whose routing hostname changed,
	// or whose generation changed (a redeploy reusing the same object key)
	// must not keep serving its previous route.
	if prev, ok := ht.workloadKeys[key]; ok && prev != route {
		ht.removeWorkload(prev)
	}

	ht.workloadKeys[key] = route
	ht.workloads[workloadID] = hostID
	hr, ok := ht.hostnames[hostname]
	if !ok {
		hr = &hostnameRoute{generations: make(map[string]sets.Set[string])}
		ht.hostnames[hostname] = hr
	}
	hr.addWorkload(workloadID, generation)
	return nil
}

func (ht *HostTracker) DeregisterWorkload(ctx context.Context, key types.NamespacedName) error {
	ht.lock.Lock()
	defer ht.lock.Unlock()

	route, ok := ht.workloadKeys[key]
	if !ok {
		return nil
	}
	delete(ht.workloadKeys, key)
	ht.removeWorkload(route)
	return nil
}

// The caller must hold ht.lock.
func (ht *HostTracker) removeWorkload(route workloadRoute) {
	delete(ht.workloads, route.workloadID)
	if hr, ok := ht.hostnames[route.hostname]; ok {
		hr.removeWorkload(route.workloadID, route.generation)
		if hr.empty() {
			delete(ht.hostnames, route.hostname)
		}
	}
}
