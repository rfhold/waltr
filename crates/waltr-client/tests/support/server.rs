pub fn server_endpoint() -> String {
    waltr_server::test_support::setup_server().base_url()
}
