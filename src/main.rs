use actix::Addr;
use actix::AsyncContext;
use actix::Message;
use actix::{Actor, Handler, StreamHandler};
use actix_web::{web, App, Error, HttpRequest, HttpResponse, HttpServer};
use actix_web_actors::ws;
use clap::{ArgAction, Parser};
use notify::Watcher;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::runtime;
use tracing::instrument;
use tracing::{event, Level};

pub static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
enum WsMessage {
    Open { path: String },
}

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
}

#[derive(Debug)]
struct GstDots {
    gstdot_path: std::path::PathBuf,
    svg_path: std::path::PathBuf,
    html_path: std::path::PathBuf,
    clients: Arc<Mutex<Vec<Addr<MyWebSocket>>>>,
    dot_watcher: Mutex<Option<notify::RecommendedWatcher>>,
    svg_watcher: Mutex<Option<notify::RecommendedWatcher>>,
    args: Args,
}

impl GstDots {
    fn new() -> Arc<Self> {
        let gstdot_path = std::env::var("GST_DEBUG_DUMP_DOT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let mut path = dirs::cache_dir().expect("Failed to find cache directory");
                path.push("gstreamer-dots");
                path
            });
        std::fs::create_dir_all(&gstdot_path).expect("Failed to create svg directory");

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
            args: Args::parse(),
            clients: Arc::new(Mutex::new(Vec::new())),
            dot_watcher: Default::default(),
            svg_watcher: Default::default(),
        });
        app.watch_dot_files();
        app.watch_svg_files();
        app.init_files();
        app.cleanup_dirs();

        app
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

    fn svg_path_from_dot_path(&self, dot_path: &std::path::Path) -> std::path::PathBuf {
        let file_name = dot_path.file_name().unwrap();

        self.svg_path.join(file_name).with_extension("svg")
    }

    fn init_files(self: &Arc<Self>) {
        event!(
            Level::DEBUG,
            "Generate svgs for dot files in {:?}",
            self.gstdot_path
        );
        let app_clone = self.clone();
        if let Ok(entries) = std::fs::read_dir(&self.gstdot_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "dot").unwrap_or(false)
                    && !app_clone.svg_path_from_dot_path(&path).exists()
                {
                    let app_clone = app_clone.clone();
                    let path = path.to_path_buf();

                    RUNTIME.spawn(async move {
                        app_clone.generate_svg(path.as_path()).await;
                    });
                }
            }
        }
    }

    fn list_svgs(&self, client: Addr<MyWebSocket>) {
        event!(Level::DEBUG, "Listing svg files in {:?}", self.svg_path);
        if let Ok(entries) = std::fs::read_dir(&self.svg_path) {
            for entry in entries.flatten() {
                let svg_path = entry.path();
                if let Some("svg") = svg_path.extension().and_then(|e| e.to_str()) {
                    client.do_send(TextMessage(
                        json!({
                            "type": "SvgCreated",
                            "path": [svg_path.strip_prefix(&self.svg_path).unwrap()]
                        })
                        .to_string(),
                    ));
                }
            }
        }
    }

    fn watch_svg_files(self: &Arc<Self>) {
        let watcher_app = self.clone();
        let mut svg_watcher = notify::recommended_watcher(move |event| match event {
            Ok(event) => watcher_app.notify_file_event(&event),
            Err(err) => event!(Level::ERROR, "watch error: {:?}", err),
        })
        .expect("Could not create svg_watcher");

        event!(Level::INFO, "Watching svg files in {:?}", self.svg_path);
        svg_watcher
            .watch(self.svg_path.as_path(), notify::RecursiveMode::Recursive)
            .unwrap();
        *self.svg_watcher.lock().unwrap() = Some(svg_watcher);
    }

    async fn generate_svg(self: &Arc<Self>, dot_path: &std::path::Path) {
        let svg_path = self.svg_path_from_dot_path(dot_path);

        event!(
            Level::DEBUG,
            "Generating svg for {dot_path:?} into {svg_path:?}"
        );
        let output = match Command::new("dot")
            .arg("-Tsvg")
            .arg(dot_path)
            .arg("-o")
            .arg(&svg_path)
            .output()
            .await
        {
            Ok(output) => output,
            Err(err) => {
                event!(Level::ERROR, "Failed to execute dot: {:?}", err);
                return;
            }
        };

        if output.status.success() {
            event!(Level::INFO, "Generated svg: {:?}", svg_path);
            let clients = self.clients.lock().unwrap();
            let clients = clients.clone();

            for client in clients.iter() {
                client.do_send(TextMessage(
                    json!({
                        "type": "SvgCreated",
                        "path": [svg_path.strip_prefix(&self.svg_path).unwrap()]
                    })
                    .to_string(),
                ));
            }
        } else {
            event!(Level::ERROR, "Failed to generate svg: {:?}", output);
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
                                notify::event::EventKind::Create(_) => {
                                    for path in event.paths.iter() {
                                        event!(Level::INFO, "File created: {:?}", path);
                                        if path.extension().map(|e| e == "dot").unwrap_or(false) {
                                            let app_clone = app_clone.clone();
                                            let path = path.to_path_buf();

                                            RUNTIME.spawn(async move {
                                                app_clone.generate_svg(path.as_path()).await;
                                            });
                                        }
                                    }
                                }
                                notify::event::EventKind::Remove(_) => {
                                    event!(Level::INFO, "File removed: {:?}", event.paths);
                                    for path in event.paths.iter() {
                                        event!(Level::INFO, "File created: {:?}", path);
                                        if path.extension().map(|e| e == "dot").unwrap_or(false) {
                                            let svg_path = app_clone.svg_path_from_dot_path(path);
                                            if let Err(e) = std::fs::remove_file(svg_path) {
                                                event!(
                                                    Level::ERROR,
                                                    "Failed to remove svg: {:?}",
                                                    e
                                                );
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

    fn notify_file_event(&self, event: &notify::event::Event) {
        let clients = self.clients.lock().unwrap();
        let clients = clients.clone();

        if event
            .paths
            .iter()
            .any(|p| p.extension().map(|e| e == "svg").unwrap_or(false))
        {
            match event.kind {
                notify::event::EventKind::Create(_) => {
                    let paths = event
                        .paths
                        .iter()
                        .map(|p| p.strip_prefix(&self.svg_path).unwrap())
                        .collect::<Vec<_>>();
                    event!(Level::INFO, "NEW SVG created: {:?}", paths);

                    for client in clients.iter() {
                        client.do_send(TextMessage(
                            json!({
                                "type": "SvgCreated",
                                "path": paths
                            })
                            .to_string(),
                        ));
                    }
                }
                notify::event::EventKind::Modify(_) => {
                    event!(Level::TRACE, "File modified: {:?}", event.paths);
                }
                notify::event::EventKind::Remove(_) => {
                    event!(Level::INFO, "File removed: {:?}", event.paths);
                    for client in clients.iter() {
                        let paths = event
                            .paths
                            .iter()
                            .map(|p| p.strip_prefix(&self.svg_path).unwrap())
                            .collect::<Vec<_>>();
                        for path in paths.iter() {
                            client.do_send(TextMessage(
                                json!({
                                    "type": "SvgRemoved",
                                    "path": path
                                })
                                .to_string(),
                            ));
                        }
                    }
                }
                _ => {
                    event!(Level::DEBUG, "File event: {:?}", event.paths);
                }
            }
        }
    }

    #[instrument(level = "trace")]
    fn add_client(&self, client: Addr<MyWebSocket>) {
        let mut clients = self.clients.lock().unwrap();

        event!(Level::INFO, "Client added: {:?}", client);
        clients.push(client.clone());
        drop(clients);

        self.list_svgs(client);
    }

    #[instrument(level = "trace")]
    fn remove_client(&self, addr: &Addr<MyWebSocket>) {
        event!(Level::INFO, "Client removed: {:?}", addr);
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|a| a != addr);
    }

    async fn run(self: &Arc<Self>) -> std::io::Result<()> {
        let app_data = web::Data::new(self.clone());
        let address = format!("{}:{}", self.args.address, self.args.port);
        event!(Level::INFO, "Starting server on http://{}", address);

        HttpServer::new(move || {
            App::new()
                .app_data(app_data.clone())
                .route("/index.html", web::get().to(index))
                .route("/", web::get().to(index))
                .service(actix_files::Files::new("/js", "./js").show_files_listing())
                .service(actix_files::Files::new("/css", "./css").show_files_listing())
                .service(actix_files::Files::new("/svg", "./.generated/svg").show_files_listing())
                .service(actix_files::Files::new("/viewer", "./.generated/html").show_files_listing())
                .route("/ws/", web::get().to(ws_index))
        })
        .bind(&address)?
        .run()
        .await
    }

    async fn generate_viewer(
        self: &Arc<Self>,
        html_file_name: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let template_content = include_str!("../templates/single_graph_template.html");

        let html_file_path = self.html_path.join(html_file_name);
        if html_file_path.exists() {
            return Ok(html_file_name.to_string());
        }

        let svg_file_path = format!("/svg/{}", html_file_name.replace(".html", ".svg"));
        let graph_html = template_content.replace("{{ svg_file_path }}", &svg_file_path);
        let graph_html = graph_html.replace("{{ TITLE }}", &html_file_name.replace(".html", ""));

        let mut file = tokio::fs::File::create(html_file_path).await?;
        file.write_all(graph_html.as_bytes()).await?;

        Ok(html_file_name.to_string())
    }
}

#[derive(Debug)]
struct MyWebSocket {
    app: Arc<GstDots>,
}

#[derive(Message)]
#[rtype(result = "()")] // Indicates that no response is expected
pub struct TextMessage(pub String);

impl Actor for MyWebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.app.add_client(ctx.address());
    }

    fn stopping(&mut self, ctx: &mut Self::Context) -> actix::Running {
        self.app.remove_client(&ctx.address());
        actix::Running::Stop
    }
}

impl Handler<TextMessage> for MyWebSocket {
    type Result = ();

    fn handle(&mut self, msg: TextMessage, ctx: &mut Self::Context) {
        // Send the text message to the WebSocket client
        ctx.text(msg.0);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for MyWebSocket {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        if let Ok(ws::Message::Text(text)) = msg {
            let message: Result<WsMessage, _> = serde_json::from_str(&text);
            event!(Level::INFO, "Message received: {:?}", message);

            if let Ok(WsMessage::Open { path }) = message {
                let app = self.app.clone();
                let html_path = path.clone();

                let client = ctx.address().clone();
                RUNTIME.spawn(async move {
                    match app.generate_viewer(&html_path).await {
                        Ok(html_file_path) => {
                            event!(Level::INFO, "Generated html: {:?}", html_file_path);
                            client.do_send(TextMessage(
                                json!({
                                    "type": "HtmlCreated",
                                    "path": html_file_path
                                })
                                .to_string(),
                            ));
                        }
                        Err(e) => event!(Level::ERROR, "Failed to generate html: {:?}", e),
                    }
                });
            }
        }
    }
}

async fn index() -> Result<actix_files::NamedFile, Error> {
    Ok(actix_files::NamedFile::open("./index.html")?)
}

async fn ws_index(
    req: HttpRequest,
    stream: web::Payload,
    data: web::Data<Arc<GstDots>>,
) -> Result<HttpResponse, Error> {
    let app = data.get_ref().clone();

    ws::start(MyWebSocket { app }, &req, stream)
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
