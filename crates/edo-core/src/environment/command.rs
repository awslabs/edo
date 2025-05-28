use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use handlebars::Handlebars;
use snafu::{ensure, ResultExt};

use crate::context::Log;
use crate::storage::Id;

use super::Environment;
use super::{error, EnvResult};

/// A Command represents a delayed series of commands to run inside of an environment.
/// All transforms should use this to define how they work.
#[derive(Clone)]
pub struct Command {
    id: Id,
    env: Environment,
    log: Log,
    interpreter: String,
    commands: Vec<String>,
    variables: HashMap<String, String>,
}

impl Command {
    pub fn new(log: &Log, id: &Id, env: &Environment) -> Self {
        Self {
            id: id.clone(),
            env: env.clone(),
            log: log.clone(),
            interpreter: "bash".into(),
            commands: Vec::new(),
            variables: HashMap::new(),
        }
    }

    pub fn set_interpreter(&mut self, interpreter: &str) {
        self.interpreter = interpreter.to_string();
    }

    pub fn set(&mut self, key: &str, value: &str) -> EnvResult<()> {
        let value = self.sub(value)?;
        self.variables.insert(key.to_string(), value);
        Ok(())
    }

    fn sub(&self, line: &str) -> EnvResult<String> {
        let current = line.to_string();
        let hg = Handlebars::new();

        hg.render_template(current.as_str(), &self.variables)
            .context(error::TemplateSnafu)
    }

    pub fn chdir(&mut self, path: &str) -> EnvResult<()> {
        self.commands.push(format!("cd {}", self.sub(path)?));
        Ok(())
    }

    pub fn pushd(&mut self, path: &str) -> EnvResult<()> {
        self.commands.push(format!("pushd {}", self.sub(path)?));
        Ok(())
    }

    pub fn popd(&mut self) {
        self.commands.push("popd".into());
    }

    pub async fn create_named_dir(&mut self, key: &str, path: &str) -> EnvResult<()> {
        let path = self.sub(path)?;
        let result = self.env.expand(Path::new(path.as_str())).await?;
        self.variables
            .insert(key.to_string(), result.to_string_lossy().to_string());
        self.commands.push(format!("mkdir -p {path}"));
        Ok(())
    }

    pub async fn create_dir(&mut self, path: &str) -> EnvResult<()> {
        let path = self.sub(path)?;
        self.commands.push(format!("mkdir -p {path}"));
        Ok(())
    }

    pub async fn remove_dir(&mut self, path: &str) -> EnvResult<()> {
        let path = self.sub(path)?;
        self.commands.push(format!("rm -r {path}"));
        Ok(())
    }

    pub async fn remove_file(&mut self, path: &str) -> EnvResult<()> {
        let path = self.sub(path)?;
        self.commands.push(format!("rm {path}"));
        Ok(())
    }

    pub async fn mv(&mut self, from: &str, to: &str) -> EnvResult<()> {
        let from = self.sub(from)?;
        let to = self.sub(to)?;
        self.commands.push(format!("mv {from} {to}"));
        Ok(())
    }

    pub async fn copy(&mut self, from: &str, to: &str) -> EnvResult<()> {
        let from = self.sub(from)?;
        let to = self.sub(to)?;
        self.commands.push(format!("cp -r {from} {to}"));
        Ok(())
    }

    pub async fn run(&mut self, cmd: &str) -> EnvResult<()> {
        let cmd = self.sub(cmd)?;
        self.commands.push(cmd);
        Ok(())
    }

    pub async fn send(&self, path: &str) -> EnvResult<()> {
        let path = self.sub(path)?;
        let dir = self.env.expand(Path::new(path.as_str())).await?;
        let status = self.env.run(&self.log, &self.id, &dir, self).await?;
        ensure!(status, error::RunSnafu);
        Ok(())
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            r#"#!/usr/bin/env {}
{}"#,
            self.interpreter,
            self.commands.join("\n")
        ))
    }
}
