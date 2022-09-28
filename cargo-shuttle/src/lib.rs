mod args;
mod client;
pub mod config;
mod factory;
mod init;
mod logger;

use std::collections::BTreeMap;
use std::fs::{read_to_string, File};
use std::io::Write;
use std::io::{self, stdout};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{anyhow, Context, Result};
pub use args::{Args, Command, DeployArgs, InitArgs, ProjectArgs, RunArgs};
use args::{AuthArgs, LoginArgs};
use cargo::core::resolver::CliFeatures;
use cargo::core::Workspace;
use cargo::ops::{PackageOpts, Packages};
use cargo_metadata::Message;
use config::RequestContext;
use crossterm::style::Stylize;
use factory::LocalFactory;
use futures::StreamExt;
use shuttle_common::{deployment, secret};
use shuttle_service::loader::{build_crate, Loader};
use tracing::trace;
use uuid::Uuid;

use crate::args::{DeploymentCommand, ProjectCommand};
use crate::client::Client;
use crate::logger::Logger;

pub struct Shuttle {
    ctx: RequestContext,
}

impl Default for Shuttle {
    fn default() -> Self {
        Self::new()
    }
}

impl Shuttle {
    pub fn new() -> Self {
        let ctx = RequestContext::load_global().unwrap();
        Self { ctx }
    }

    pub async fn run(mut self, mut args: Args) -> Result<CommandOutcome> {
        trace!("running local client");
        if matches!(
            args.cmd,
            Command::Deploy(..)
                | Command::Deployment(..)
                | Command::Project(..)
                | Command::Delete
                | Command::Secrets
                | Command::Status
                | Command::Logs { .. }
                | Command::Run(..)
        ) {
            self.load_project(&mut args.project_args)?;
        }

        self.ctx.set_api_url(args.api_url);

        let mut client = Client::new(self.ctx.api_url());
        client.set_api_key(self.ctx.api_key()?);

        match args.cmd {
            Command::Deploy(deploy_args) => {
                return self.deploy(deploy_args, &client).await;
            }
            Command::Init(init_args) => self.init(init_args).await,
            Command::Status => self.status(&client).await,
            Command::Logs { id, follow } => self.logs(&client, id, follow).await,
            Command::Deployment(DeploymentCommand::List) => self.deployments_list(&client).await,
            Command::Deployment(DeploymentCommand::Status { id }) => {
                self.deployment_get(&client, id).await
            }
            Command::Delete => self.delete(&client).await,
            Command::Secrets => self.secrets(&client).await,
            Command::Auth(auth_args) => self.auth(auth_args, &client).await,
            Command::Login(login_args) => self.login(login_args).await,
            Command::Run(run_args) => self.local_run(run_args).await,
            Command::Project(ProjectCommand::New) => self.project_create(&client).await,
            Command::Project(ProjectCommand::Status) => self.project_status(&client).await,
            Command::Project(ProjectCommand::Rm) => self.project_delete(&client).await,
        }
        .map(|_| CommandOutcome::Ok)
    }

    async fn init(&self, args: InitArgs) -> Result<()> {
        // Interface with cargo to initialize new lib package for shuttle
        let path = args.path.clone();
        init::cargo_init(path.clone())?;

        let framework = init::get_framework(&args);
        init::cargo_shuttle_init(path, framework)?;

        Ok(())
    }

    fn find_root_directory(dir: &Path) -> Option<PathBuf> {
        dir.ancestors()
            .find(|ancestor| ancestor.join("Cargo.toml").exists())
            .map(|path| path.to_path_buf())
    }

    pub fn load_project(&mut self, project_args: &mut ProjectArgs) -> Result<()> {
        trace!("loading project arguments: {project_args:?}");
        let root_directory_path = Self::find_root_directory(&project_args.working_directory);

        if let Some(working_directory) = root_directory_path {
            project_args.working_directory = working_directory;
        } else {
            return Err(anyhow!("Could not locate the root of a cargo project. Are you inside a cargo project? You can also use `--working-directory` to locate your cargo project."));
        }

        self.ctx.load_local(project_args)
    }

    async fn login(&mut self, login_args: LoginArgs) -> Result<()> {
        let api_key_str = login_args.api_key.unwrap_or_else(|| {
            let url = "https://shuttle.rs/login";

            let _ = webbrowser::open(url);

            println!("If your browser did not automatically open, go to {url}");
            print!("Enter Api Key: ");

            stdout().flush().unwrap();

            let mut input = String::new();

            io::stdin().read_line(&mut input).unwrap();

            input
        });

        let api_key = api_key_str.trim().parse()?;

        self.ctx.set_api_key(api_key)?;

        Ok(())
    }

    async fn auth(&mut self, auth_args: AuthArgs, client: &Client) -> Result<()> {
        let user = client.auth(auth_args.username).await?;

        self.ctx.set_api_key(user.key)?;

        println!("User authorized!!!");
        println!("Run `cargo shuttle init --help` next");

        Ok(())
    }

    async fn delete(&self, client: &Client) -> Result<()> {
        let service = client.delete_service(self.ctx.project_name()).await?;

        println!(
            r#"{}
{}"#,
            "Successfully deleted service".bold(),
            service
        );

        Ok(())
    }

    async fn status(&self, client: &Client) -> Result<()> {
        let summary = client.get_service_summary(self.ctx.project_name()).await?;

        println!("{summary}");

        Ok(())
    }

    async fn secrets(&self, client: &Client) -> Result<()> {
        let secrets = client.get_secrets(self.ctx.project_name()).await?;
        let table = secret::get_table(&secrets);

        println!("{table}");

        Ok(())
    }

    async fn logs(&self, client: &Client, id: Option<Uuid>, follow: bool) -> Result<()> {
        let id = if let Some(id) = id {
            id
        } else {
            let summary = client.get_service_summary(self.ctx.project_name()).await?;

            if let Some(deployment) = summary.deployment {
                deployment.id
            } else {
                return Err(anyhow!("could not automatically find a running deployment for '{}'. Try passing a deployment ID manually", self.ctx.project_name()));
            }
        };

        if follow {
            let mut stream = client
                .get_runtime_logs_ws(self.ctx.project_name(), &id)
                .await?;

            while let Some(Ok(msg)) = stream.next().await {
                if let tokio_tungstenite::tungstenite::Message::Text(line) = msg {
                    let log_item: shuttle_common::log::Item =
                        serde_json::from_str(&line).expect("to parse log line");
                    println!("{log_item}")
                }
            }
        } else {
            let logs = client
                .get_runtime_logs(self.ctx.project_name(), &id)
                .await?;

            for log in logs.into_iter() {
                println!("{log}");
            }
        }

        Ok(())
    }

    async fn deployments_list(&self, client: &Client) -> Result<()> {
        let details = client.get_service_details(self.ctx.project_name()).await?;

        println!("{details}");

        Ok(())
    }

    async fn deployment_get(&self, client: &Client, deployment_id: Uuid) -> Result<()> {
        let deployment = client
            .get_deployment_details(self.ctx.project_name(), &deployment_id)
            .await?;

        println!("{deployment}");

        Ok(())
    }

    async fn local_run(&self, run_args: RunArgs) -> Result<()> {
        trace!("starting a local run for a service: {run_args:?}");

        let (tx, rx): (crossbeam_channel::Sender<Message>, _) = crossbeam_channel::bounded(0);
        tokio::spawn(async move {
            while let Ok(message) = rx.recv() {
                match message {
                    Message::TextLine(line) => println!("{line}"),
                    Message::CompilerMessage(message) => {
                        if let Some(rendered) = message.message.rendered {
                            println!("{rendered}");
                        }
                    }
                    _ => {}
                }
            }
        });

        let working_directory = self.ctx.working_directory();

        trace!("building project");
        println!(
            "{:>12} {}",
            "Building".bold().green(),
            working_directory.display()
        );
        let so_path = build_crate(working_directory, tx).await?;

        trace!("loading secrets");
        let secrets_path = working_directory.join("Secrets.toml");

        let secrets: BTreeMap<String, String> =
            if let Ok(secrets_str) = read_to_string(secrets_path) {
                let secrets: BTreeMap<String, String> =
                    secrets_str.parse::<toml::Value>()?.try_into()?;

                trace!(keys = ?secrets.keys(), "available secrets");

                secrets
            } else {
                trace!("no Secrets.toml was found");
                Default::default()
            };

        let loader = Loader::from_so_file(so_path)?;

        let mut factory = LocalFactory::new(self.ctx.project_name().clone(), secrets)?;
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), run_args.port);

        trace!("loading project");
        println!(
            "\n{:>12} {} on http://{}",
            "Starting".bold().green(),
            self.ctx.project_name(),
            addr
        );
        let logger = Box::new(Logger::new());
        let (handle, so) = loader.load(&mut factory, addr, logger).await?;

        handle.await??;

        tokio::spawn(async move {
            trace!("closing so file");
            so.close().unwrap();
        });

        Ok(())
    }

    async fn deploy(&self, args: DeployArgs, client: &Client) -> Result<CommandOutcome> {
        let package_file = self
            .run_cargo_package(args.allow_dirty)
            .context("failed to package cargo project")?;

        let deployment = client
            .deploy(package_file, self.ctx.project_name(), args.no_test)
            .await?;

        println!();
        println!("{deployment}");

        let mut stream = client
            .get_build_logs_ws(self.ctx.project_name(), &deployment.id)
            .await?;

        while let Some(Ok(msg)) = stream.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(line) = msg {
                println!("{line}");
            }
        }

        let service = client.get_service_summary(self.ctx.project_name()).await?;

        // A deployment will only exist if there is currently one in the running state
        if let Some(ref new_deployment) = service.deployment {
            if new_deployment.id != deployment.id {
                println!(
                    "Deployment has not entered the running state so kept previous deployment up"
                );

                return Ok(CommandOutcome::DeploymentFailure);
            }

            println!("{service}");

            Ok(match new_deployment.state {
                deployment::State::Crashed => CommandOutcome::DeploymentFailure,
                _ => CommandOutcome::Ok,
            })
        } else {
            println!("Deployment has not entered the running state");

            Ok(CommandOutcome::DeploymentFailure)
        }
    }

    async fn project_create(&self, client: &Client) -> Result<()> {
        let project = client.create_project(self.ctx.project_name()).await?;

        println!("{project}");

        Ok(())
    }

    async fn project_status(&self, client: &Client) -> Result<()> {
        let project = client.get_project(self.ctx.project_name()).await?;

        println!("{project}");

        Ok(())
    }

    async fn project_delete(&self, client: &Client) -> Result<()> {
        client.delete_project(self.ctx.project_name()).await?;

        println!("Project has been deleted");

        Ok(())
    }

    // Packages the cargo project and returns a File to that file
    fn run_cargo_package(&self, allow_dirty: bool) -> Result<File> {
        let config = cargo::util::config::Config::default()?;

        let working_directory = self.ctx.working_directory();
        let path = working_directory.join("Cargo.toml");

        let ws = Workspace::new(&path, &config)?;
        let opts = PackageOpts {
            config: &config,
            list: false,
            check_metadata: true,
            allow_dirty,
            keep_going: false,
            verify: false,
            jobs: None,
            to_package: Packages::Default,
            targets: vec![],
            cli_features: CliFeatures {
                features: Rc::new(Default::default()),
                all_features: false,
                uses_default_features: true,
            },
        };

        let locks = cargo::ops::package(&ws, &opts)?.expect("unwrap ok here");
        let owned = locks.get(0).unwrap().file().try_clone()?;
        Ok(owned)
    }
}

pub enum CommandOutcome {
    Ok,
    DeploymentFailure,
}

#[cfg(test)]
mod tests {
    use crate::args::ProjectArgs;
    use crate::Shuttle;
    use std::path::PathBuf;

    fn path_from_workspace_root(path: &str) -> PathBuf {
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("..")
            .join(path)
    }

    #[test]
    fn find_root_directory_returns_proper_directory() {
        let working_directory = path_from_workspace_root("examples/axum/hello-world/src");

        let root_dir = Shuttle::find_root_directory(&working_directory).unwrap();

        assert_eq!(
            root_dir,
            path_from_workspace_root("examples/axum/hello-world/")
        );
    }

    #[test]
    fn load_project_returns_proper_working_directory_in_project_args() {
        let mut project_args = ProjectArgs {
            working_directory: path_from_workspace_root("examples/axum/hello-world/src"),
            name: None,
        };

        let mut shuttle = Shuttle::new();
        Shuttle::load_project(&mut shuttle, &mut project_args).unwrap();

        assert_eq!(
            project_args.working_directory,
            path_from_workspace_root("examples/axum/hello-world/")
        );
    }
}
