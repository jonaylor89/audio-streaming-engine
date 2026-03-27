use once_cell::sync::Lazy;
use streaming_engine::{
    config::get_configuration,
    startup::Application,
    telemetry::{get_subscriber, init_subscriber},
};
use tokio::task::JoinHandle;

static TRACING: Lazy<()> = Lazy::new(|| {
    let default_filter_level = "info".to_string();
    let subscriber_name = "test".to_string();

    if std::env::var("TEST_LOG").is_ok() {
        let subscriber = get_subscriber(subscriber_name, default_filter_level, std::io::stdout);
        init_subscriber(subscriber);
    } else {
        let subscriber = get_subscriber(subscriber_name, default_filter_level, std::io::sink);
        init_subscriber(subscriber);
    }
});

pub struct TestApp {
    pub address: String,
    pub port: u16,
    pub api_client: reqwest::Client,
    server_handle: JoinHandle<()>,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.server_handle.abort();
    }
}

pub async fn spawn_app() -> TestApp {
    Lazy::force(&TRACING);

    let configuration = {
        let mut c = get_configuration().expect("Failed to read configuration");
        c.port = 0;

        c
    };

    let application = Application::build(configuration.clone())
        .await
        .expect("Failed to build application");

    let application_port = application.port;
    let address = format!("http://localhost:{}", application_port);

    let server_handle = tokio::spawn(async move {
        application
            .run_until_stopped()
            .await
            .expect("test server exited unexpectedly");
    });

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    TestApp {
        address,
        port: application_port,
        api_client: client,
        server_handle,
    }
}
