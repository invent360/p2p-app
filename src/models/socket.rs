use actix::prelude::*;
use actix_web::{web, App, HttpServer, HttpRequest, HttpResponse, Error};
use actix_web_actors::ws;
struct MyWs;

// WebSocket handler
pub async fn ws_index(req: HttpRequest, stream: web::Payload) -> Result<HttpResponse, Error> {
    ws::start(MyWs, &req, stream)
}
impl Actor for MyWs {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        tracing::info!("WebSocket connection established");
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for MyWs {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Text(text)) => {
                tracing::info!("Received WebSocket message: {}", text);
                ctx.text(text);
            }
            _ => (),
        }
    }
}
