use waltr::grpc::waltr::config::v1::{
    Configuration, GetConfigRequest, Provider, ProviderType,
    config_service_client::ConfigServiceClient,
};
use waltr::grpc::waltr::session::v1::session_service_client::SessionServiceClient;
use waltr::grpc::waltr::status::v1::status_service_client::StatusServiceClient;
use waltr_server::test_support::TestServer;
use tonic::transport::{Channel, Endpoint};

pub(crate) async fn config_client(
    server: &TestServer,
) -> Result<ConfigServiceClient<Channel>, Box<dyn std::error::Error>> {
    config_client_for_url(server.base_url()).await
}

pub(crate) async fn config_client_for_addr(
    addr: std::net::SocketAddr,
) -> Result<ConfigServiceClient<Channel>, Box<dyn std::error::Error>> {
    config_client_for_url(format!("http://{addr}")).await
}

pub(crate) async fn status_client(
    server: &TestServer,
) -> Result<StatusServiceClient<Channel>, Box<dyn std::error::Error>> {
    let channel = Endpoint::from_shared(server.base_url())?.connect().await?;
    Ok(StatusServiceClient::new(channel))
}

pub(crate) async fn session_client(
    server: &TestServer,
) -> Result<SessionServiceClient<Channel>, Box<dyn std::error::Error>> {
    let channel = Endpoint::from_shared(server.base_url())?.connect().await?;
    Ok(SessionServiceClient::new(channel))
}

pub(crate) async fn get_config(
    client: &mut ConfigServiceClient<Channel>,
) -> Result<Configuration, Box<dyn std::error::Error>> {
    Ok(client
        .get_config(GetConfigRequest::default())
        .await?
        .into_inner()
        .config
        .expect("config should be present"))
}

pub(crate) fn config_with_provider(name: &str) -> Configuration {
    Configuration {
        providers: std::collections::HashMap::from([(
            name.to_owned(),
            Provider {
                r#type: ProviderType::Anthropic as i32,
                name: name.to_owned(),
                url: "https://api.anthropic.com".to_owned(),
                ..Provider::default()
            },
        )]),
        ..Configuration::default()
    }
}

async fn config_client_for_url(
    url: String,
) -> Result<ConfigServiceClient<Channel>, Box<dyn std::error::Error>> {
    let channel = Endpoint::from_shared(url)?.connect().await?;
    Ok(ConfigServiceClient::new(channel))
}
