# SSE Component

## Running this locally

Push the wasm component in some registry.
Refer to these in the `workloaddeployment.yaml`.

Start a connection:

`curl -N -H "Accept: text/event-stream" -H "Host: sse-demo" http://localhost:8888/chatMessage/323 `

Send a message:

```
curl -X POST -H "Host: sse-demo"     -H "Content-Type: application/json"     http://localhost:8888/push     -d '{"scope_key": "chatMessage/323", "data": "{\"id\": 1, \"message\": \"hello\"}"}'
```

List connections:

`curl -H "Host: sse-demo" http://localhost:8888/connections/chatMessage/323`


