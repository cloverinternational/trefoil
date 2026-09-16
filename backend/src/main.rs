use trefoil_backend::app;

#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("failed to bind port 8080");
    axum::serve(listener, app(trefoil_backend::AppState::default()))
        .await
        .expect("server error");
}
