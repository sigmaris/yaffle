use toshi::Client;
use hyper::client::connect::Connect;
use toshi::HyperToshi;

#[tokio::main]
async fn main() {
    let client = hyper::Client::default();
    let c = HyperToshi::with_client("http://localhost:8080", client);
    create_index(&c);
}

fn create_index<C>(client: &HyperToshi<C>)
where C: Connect + Clone + Send + Sync + 'static, {
    println!("Hello, world!");
}
