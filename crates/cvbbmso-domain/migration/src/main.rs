//! Runs the `SeaORM` migration CLI for the exam database.

#[tokio::main]
async fn main() {
    migration::cli::run_cli(migration::Migrator).await;
}
