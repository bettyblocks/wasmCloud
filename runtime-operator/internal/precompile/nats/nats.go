// Package nats provides NATS JetStream-backed implementations of the
// precompile queue interfaces.
package nats

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"

	"go.wasmcloud.dev/runtime-operator/v2/internal/precompile"
)

const (
	// DefaultStream is the JetStream stream the worker pool consumes from.
	DefaultStream = "wasmcloud-precompile"
	// jobSubjectFormat is the per-target subject jobs are published to.
	// Workers consume from a single durable consumer that subscribes to
	// `wasmcloud.precompile.jobs.>`, so the subject suffix is informational.
	jobSubjectFormat = "wasmcloud.precompile.jobs.%s"
	// completionSubject is the wildcard subject the operator subscribes to
	// for completion events; the worker publishes per-Artifact suffixes
	// like `wasmcloud.precompile.completion.{namespace}.{name}`.
	completionSubject = "wasmcloud.precompile.completion.>"
)

// Producer publishes PrecompileJob messages onto the JetStream stream.
type Producer struct {
	js     jetstream.JetStream
	stream string
}

// NewProducer constructs a Producer bound to a stream that the caller has
// already created (typically by the chart bootstrap Job).
func NewProducer(nc *nats.Conn, stream string) (*Producer, error) {
	js, err := jetstream.New(nc)
	if err != nil {
		return nil, fmt.Errorf("jetstream.New: %w", err)
	}
	return &Producer{js: js, stream: stream}, nil
}

// Submit publishes a job. The subject encodes the target so a future
// per-target consumer split is possible without changing call sites.
func (p *Producer) Submit(ctx context.Context, job precompile.PrecompileJob) error {
	payload, err := json.Marshal(job)
	if err != nil {
		return fmt.Errorf("marshal job: %w", err)
	}
	subject := fmt.Sprintf(jobSubjectFormat, job.Target)
	if _, err := p.js.Publish(ctx, subject, payload); err != nil {
		return fmt.Errorf("publish to %q: %w", subject, err)
	}
	return nil
}

// Consumer subscribes to completion events. Implements
// precompile.CompletionConsumer using a plain core-NATS subscription rather
// than JetStream — completion events are fire-and-forget; a missed event
// just delays the next status patch until the next reconcile.
type Consumer struct {
	nc *nats.Conn
}

// NewConsumer wraps a NATS connection.
func NewConsumer(nc *nats.Conn) *Consumer {
	return &Consumer{nc: nc}
}

// Subscribe runs until ctx is cancelled. handler is called for each
// successfully-decoded completion event; decode errors are logged via the
// nats client's default error handler.
func (c *Consumer) Subscribe(ctx context.Context, handler func(precompile.PrecompileCompletion)) error {
	sub, err := c.nc.Subscribe(completionSubject, func(msg *nats.Msg) {
		var ev precompile.PrecompileCompletion
		if err := json.Unmarshal(msg.Data, &ev); err != nil {
			// Bad payload — drop and move on; no recovery possible.
			return
		}
		handler(ev)
	})
	if err != nil {
		return fmt.Errorf("subscribe: %w", err)
	}
	go func() {
		<-ctx.Done()
		_ = sub.Unsubscribe()
	}()
	return nil
}
