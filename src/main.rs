use actix::Addr;
use actix::AsyncContext;
use actix::Message;
use actix::{Actor, Handler, StreamHandler};
use actix_web::{web, App, Error, HttpRequest, HttpResponse, HttpServer};
use actix_web_actors::ws;
use actix_web_static_files::ResourceFiles;
use clap::{ArgAction, Parser};
use notify::Watcher;
use once_cell::sync::Lazy;
use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::runtime;
use tracing::error;
use tracing::instrument;
use tracing::{event, Level};

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

pub static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

/// Server configuration
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Server address
    #[arg(short, long, default_value = "0.0.0.0", action = ArgAction::Set)]
    address: String,

    /// Server port
    #[arg(short, long, default_value_t = 3000, action = ArgAction::Set)]
    port: u16,

    /// Server port
    #[arg(short, long, action = ArgAction::Set)]
    dotdir: Option<String>,
}

#[derive(Debug)]
struct GstDots {
    gstdot_path: std::path::PathBuf,
    svg_path: std::path::PathBuf,
    html_path: std::path::PathBuf,
    clients: Arc<Mutex<Vec<Addr<WebSocket>>>>,
    dot_watcher: Mutex<Option<notify::RecommendedWatcher>>,
    args: Args,
}

impl GstDots {
    fn new() -> Arc<Self> {
        let args = Args::parse();
        let gstdot_path = args
            .dotdir
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let mut path = dirs::cache_dir().expect("Failed to find cache directory");
                path.push("gstreamer-dots");
                path
            });
        std::fs::create_dir_all(&gstdot_path).expect("Failed to create dot directory");

        let mut svg_path = std::env::current_dir().unwrap();
        svg_path.push(".generated/svg");
        std::fs::create_dir_all(&svg_path).expect("Failed to create svg directory");

        let mut html_path = std::env::current_dir().unwrap();
        html_path.push(".generated/html/");
        std::fs::create_dir_all(&html_path).expect("Failed to create svg directory");

        let app = Arc::new(Self {
            gstdot_path: gstdot_path.clone(),
            svg_path,
            html_path,
            args,
            clients: Arc::new(Mutex::new(Vec::new())),
            dot_watcher: Default::default(),
        });
        app.watch_dot_files();
        app.cleanup_dirs();

        app
    }

    fn relative_dot_path(&self, dot_path: &Path) -> String {
        dot_path
            .strip_prefix(&self.gstdot_path)
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    fn cleanup_dirs(self: &Arc<Self>) {
        for (dir, ext) in &[(&self.svg_path, "svg"), (&self.html_path, "html")] {
            let entries = std::fs::read_dir(dir).expect("Could not read svg directory");
            for entry in entries.flatten() {
                let path = entry.path();
                if self.dot_path_for_file(&path).exists() {
                    event!(Level::DEBUG, "Keeping {ext}: {path:?}");
                    continue;
                }

                if path.extension().map(|e| &e == ext).unwrap_or(false) {
                    if let Err(e) = std::fs::remove_file(&path) {
                        event!(Level::ERROR, "Failed to remove {ext}: {:?}", e);
                    } else {
                        event!(
                            Level::INFO,
                            "Removed {ext}: {path:?}, {:?} does not exist",
                            self.dot_path_for_file(&path)
                        );
                    }
                }
            }
        }
    }

    fn dot_path_for_file(&self, path: &std::path::Path) -> std::path::PathBuf {
        let file_name = path.file_name().unwrap();

        self.gstdot_path.join(file_name).with_extension("dot")
    }

    fn modify_time(&self, path: &std::path::Path) -> u128 {
        self.dot_path_for_file(path)
            .metadata()
            .map(|m| m.modified().unwrap_or(std::time::UNIX_EPOCH))
            .unwrap_or(std::time::UNIX_EPOCH)
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }

    fn collect_dot_files(path: &PathBuf, entries: &mut Vec<(PathBuf, SystemTime)>) {
        if let Ok(read_dir) = std::fs::read_dir(path) {
            for entry in read_dir.flatten() {
                let dot_path = entry.path();
                if dot_path.is_dir() {
                    // Recursively call this function if the path is a directory
                    Self::collect_dot_files(&dot_path, entries);
                } else {
                    // Process only `.dot` files
                    if dot_path.extension().and_then(|e| e.to_str()) == Some("dot") {
                        if let Ok(metadata) = dot_path.metadata() {
                            if let Ok(modified) = metadata.modified() {
                                entries.push((dot_path, modified));
                            }
                        }
                    }
                }
            }
        }
    }

    fn list_dots(&self, client: Addr<WebSocket>) {
        event!(Level::DEBUG, "Listing dot files in {:?}", self.gstdot_path);
        let mut entries: Vec<(PathBuf, SystemTime)> = Vec::new();

        let start_path = PathBuf::from(&self.gstdot_path);
        Self::collect_dot_files(&start_path, &mut entries);

        entries.sort_by(|a, b| a.1.cmp(&b.1));

        for (dot_path, _) in entries {
            let content = match std::fs::read_to_string(&dot_path) {
                Ok(c) => c,
                Err(_) => {
                    event!(Level::ERROR, "===>Error reading file: {:?}", dot_path);
                    continue;
                }
            };
            if content.is_empty() {
                event!(Level::ERROR, "===>Empty file: {:?}", dot_path);
                continue;
            }

            let name = self.relative_dot_path(&dot_path);
            event!(Level::INFO, "Sending `{name}` to client: {client:?}");
            client.do_send(TextMessage(
                json!({
                    "type": "NewDot",
                    "name": name,
                    "content": content,
                    "creation_time": self.modify_time(&dot_path),
                })
                .to_string(),
            ));
        }
    }

    fn watch_dot_files(self: &Arc<Self>) {
        let app_clone = self.clone();
        let mut dot_watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                match event {
                    Ok(event) => {
                        if event
                            .paths
                            .iter()
                            .any(|p| p.extension().map(|e| e == "dot").unwrap_or(false))
                        {
                            match event.kind {
                                notify::event::EventKind::Modify(notify::event::ModifyKind::Data(notify::event::DataChange::Content)) => {
                                    for path in event.paths.iter() {
                                        event!(Level::INFO, "File created: {:?}", path);
                                        if path.extension().map(|e| e == "dot").unwrap_or(false) {
                                            let path = path.to_path_buf();
                                            let clients = app_clone.clients.lock().unwrap();
                                            let clients = clients.clone();

                                            for client in clients.iter() {
                                                let name = app_clone.relative_dot_path(&path);
                                                event!(Level::DEBUG, "Sending {name} to client: {client:?}");
                                                match std::fs::read_to_string(&path) {
                                                    Ok(content) => client.do_send(TextMessage(
                                                        json!({
                                                            "type": "NewDot",
                                                            "name": name,
                                                            "content": content,
                                                            "creation_time": app_clone.modify_time(&event.paths[0]),
                                                        })
                                                        .to_string(),
                                                    )),
                                                    Err(err) => error!("Could not read file {path:?}: {err:?}"),
                                                }
                                            }
                                        }
                                    }
                                }
                                notify::event::EventKind::Remove(_) => {
                                    event!(Level::INFO, "File removed: {:?}", event.paths);
                                    for path in event.paths.iter() {
                                        event!(Level::INFO, "File created: {:?}", path);
                                        if path.extension().map(|e| e == "dot").unwrap_or(false) {
                                            let path = path.to_path_buf();
                                            let clients = app_clone.clients.lock().unwrap();
                                            let clients = clients.clone();

                                            for client in clients.iter() {
                                                event!(Level::INFO, "Sending to client: {:?}", client);
                                                client.do_send(TextMessage(
                                                    json!({
                                                        "type": "DotRemoved",
                                                        "name": path.file_name().unwrap().to_str().unwrap(),
                                                        "creation_time": app_clone.modify_time(&event.paths[0]),
                                                    })
                                                    .to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }
                                _ => (),
                            }
                        }
                    }
                    Err(err) => event!(Level::ERROR, "watch error: {:?}", err),
                }
            })
            .expect("Could not create dot_watcher");

        event!(Level::INFO, "Watching dot files in {:?}", self.gstdot_path);
        dot_watcher
            .watch(self.gstdot_path.as_path(), notify::RecursiveMode::Recursive)
            .unwrap();
        *self.dot_watcher.lock().unwrap() = Some(dot_watcher);
    }

    #[instrument(level = "trace")]
    fn add_client(&self, client: Addr<WebSocket>) {
        let mut clients = self.clients.lock().unwrap();

        event!(Level::INFO, "Client added: {:?}", client);
        clients.push(client.clone());
        drop(clients);

        self.list_dots(client);
    }

    #[instrument(level = "trace")]
    fn remove_client(&self, addr: &Addr<WebSocket>) {
        event!(Level::INFO, "Client removed: {:?}", addr);
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|a| a != addr);
    }

    async fn run(self: &Arc<Self>) -> std::io::Result<()> {
        let app_data = web::Data::new(self.clone());
        let address = format!("{}:{}", self.args.address, self.args.port);
        event!(Level::INFO, "Starting server on http://{}", address);

        HttpServer::new(move || {
            let generated = generate();
            App::new()
                .app_data(app_data.clone())
                .route("/ws/", web::get().to(ws_index))
                .service(actix_files::Files::new("/svg", "./.generated/svg").show_files_listing())
                .service(
                    actix_files::Files::new("/viewer", "./.generated/html").show_files_listing(),
                )
                .service(ResourceFiles::new("/", generated))
        })
        .bind(&address)?
        .run()
        .await
    }
}

#[derive(Debug)]
struct WebSocket {
    app: Arc<GstDots>,
}

#[derive(Message)]
#[rtype(result = "()")] // Indicates that no response is expected
pub struct TextMessage(pub String);

impl Actor for WebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.app.add_client(ctx.address());
    }

    fn stopping(&mut self, ctx: &mut Self::Context) -> actix::Running {
        self.app.remove_client(&ctx.address());
        actix::Running::Stop
    }
}

impl Handler<TextMessage> for WebSocket {
    type Result = ();

    fn handle(&mut self, msg: TextMessage, ctx: &mut Self::Context) {
        // Send the text message to the WebSocket client
        ctx.text(msg.0);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WebSocket {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, _ctx: &mut Self::Context) {
        if let Ok(ws::Message::Text(text)) = msg {
            event!(Level::INFO, "Message received: {:?}", text);
        }
    }
}

async fn ws_index(
    req: HttpRequest,
    stream: web::Payload,
    data: web::Data<Arc<GstDots>>,
) -> Result<HttpResponse, Error> {
    let app = data.get_ref().clone();

    ws::start(WebSocket { app }, &req, stream)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .compact()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_env_filter(
            tracing_subscriber::filter::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::filter::EnvFilter::new("warn,gstdots=info")
            }),
        )
        .init();

    // test alternate formatting
    struct Object {
        name: String,
        properties: HashMap<String, String>,
        b: &'static str,
    }

    impl fmt::Debug for Object {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "{}\n\nproperties: {:?}, b: {} }}",
                self.name, self.properties, self.b
            )
        }
    }
    GstDots::new().run().await
}
