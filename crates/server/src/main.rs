use server::{get_settings, setup_logger, Application};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = get_settings()?;
    setup_logger(settings.level.clone())?;
    let application = Application::build(settings).await?;

    application.run_until_stopped().await?;
    Ok(())
}
