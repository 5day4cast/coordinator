use clap::Parser;
use coordinator::{get_settings_with_cli, setup_logger, Application, Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    if let Some(Command::Admin(admin)) = cli.command.take() {
        return coordinator::admin_cli::run(admin).await;
    }
    let settings: coordinator::config::Settings = get_settings_with_cli(cli.into())?;
    setup_logger(settings.level.clone(), vec![String::from("hyper")])?;
    let application = Application::build(settings).await?;

    application.run_until_stopped().await?;
    Ok(())
}
