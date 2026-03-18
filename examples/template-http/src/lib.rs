use anyhow::Result;
use component_otel::{Tracer, SpanKind};

mod bindings {
    wit_bindgen::generate!({
        generate_all,
        with: {
            "wasi:otel/types@0.2.0-rc.1": component_otel::bindings::wasi::otel::types,
            "wasi:otel/tracing@0.2.0-rc.1": component_otel::bindings::wasi::otel::tracing,
            "wasi:otel/logs@0.2.0-rc.1": component_otel::bindings::wasi::otel::logs,
            "wasi:clocks/wall-clock@0.2.0": component_otel::bindings::wasi::clocks::wall_clock,
        },
    });
}

use bindings::{
    exports::wasi::http::incoming_handler::Guest,
    wasi::{
        http::types::{
            Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
        },
        logging::logging::{log, Level},
    },
};

struct Component;

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        match handle_request(&request) {
            Ok(body) => {
                let response = OutgoingResponse::new(Fields::new());
                response.set_status_code(200).unwrap();
                let out_body = response.body().unwrap();
                ResponseOutparam::set(response_out, Ok(response));

                let stream = out_body.write().unwrap();
                stream.blocking_write_and_flush(body.as_bytes()).unwrap();
                drop(stream);
                OutgoingBody::finish(out_body, None).unwrap();
            }
            Err(e) => {
                log(Level::Error, "", &format!("Error: {e}"));
                let response = OutgoingResponse::new(Fields::new());
                response.set_status_code(500).unwrap();
                let out_body = response.body().unwrap();
                ResponseOutparam::set(response_out, Ok(response));

                let stream = out_body.write().unwrap();
                stream
                    .blocking_write_and_flush(format!("Internal error: {e}").as_bytes())
                    .unwrap();
                drop(stream);
                OutgoingBody::finish(out_body, None).unwrap();
            }
        }
    }
}

fn handle_request(_request: &IncomingRequest) -> Result<String> {
    let tracer = Tracer::from_host("template-http", "0.1.0");
    let _root = tracer.span("handle-request")
        .kind(SpanKind::Server)
        .attr("http.method", "GET")
        .start();

    tracer.log_info("processing request");

    // Example: child span for some operation
    let result = {
        let _span = tracer.span("do-work")
            .kind(SpanKind::Internal)
            .attr("operation", "example")
            .start();

        // Your business logic here
        "Hello from wasmCloud with OTEL tracing!\n".to_string()
    }; // do-work span ends here

    tracer.log_info("request complete");
    Ok(result)
} // handle-request span ends here

bindings::export!(Component with_types_in bindings);
