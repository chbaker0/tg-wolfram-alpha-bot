#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    println!("Hello, world!");
    Err(eyre::eyre!("Not implemented..."))
}
