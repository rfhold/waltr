use waltr::grpc::waltr::catalog::v1::catalog_service_client::CatalogServiceClient;
use waltr::grpc::waltr::config::v1::Configuration;
use waltr::grpc::waltr::resource::v1::{
    GetResourceCatalogRequest, ResourceCatalog, resource_service_client::ResourceServiceClient,
};
use waltr::grpc::waltr::session::v1::session_service_client::SessionServiceClient;
use waltr::grpc::waltr::status::v1::status_service_client::StatusServiceClient;
use waltr_server::test_support::TestServer;
use tonic::transport::{Channel, Endpoint};

pub async fn resource_client(
    server: &TestServer,
) -> Result<ResourceServiceClient<Channel>, Box<dyn std::error::Error>> {
    resource_client_for_url(server.base_url()).await
}

pub async fn resource_client_for_addr(
    addr: std::net::SocketAddr,
) -> Result<ResourceServiceClient<Channel>, Box<dyn std::error::Error>> {
    resource_client_for_url(format!("http://{addr}")).await
}

pub async fn status_client(
    server: &TestServer,
) -> Result<StatusServiceClient<Channel>, Box<dyn std::error::Error>> {
    let channel = Endpoint::from_shared(server.base_url())?.connect().await?;
    Ok(StatusServiceClient::new(channel))
}

pub async fn catalog_client(
    server: &TestServer,
) -> Result<CatalogServiceClient<Channel>, Box<dyn std::error::Error>> {
    let channel = Endpoint::from_shared(server.base_url())?.connect().await?;
    Ok(CatalogServiceClient::new(channel))
}

pub async fn session_client(
    server: &TestServer,
) -> Result<SessionServiceClient<Channel>, Box<dyn std::error::Error>> {
    let channel = Endpoint::from_shared(server.base_url())?.connect().await?;
    Ok(SessionServiceClient::new(channel))
}

pub async fn get_catalog(
    client: &mut ResourceServiceClient<Channel>,
) -> Result<ResourceCatalog, Box<dyn std::error::Error>> {
    Ok(client
        .get_resource_catalog(GetResourceCatalogRequest {})
        .await?
        .into_inner()
        .catalog
        .expect("resource catalog should be present"))
}

pub async fn get_config(
    client: &mut ResourceServiceClient<Channel>,
) -> Result<Configuration, Box<dyn std::error::Error>> {
    Ok(get_catalog(client)
        .await?
        .effective_configuration
        .expect("effective configuration should be present"))
}

async fn resource_client_for_url(
    url: String,
) -> Result<ResourceServiceClient<Channel>, Box<dyn std::error::Error>> {
    let channel = Endpoint::from_shared(url)?.connect().await?;
    Ok(ResourceServiceClient::new(channel))
}
