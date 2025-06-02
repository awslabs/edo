use crate::context::Log;
use std::ffi::OsString;
use std::io::Result;
use std::os::fd::IntoRawFd;
use std::path::Path;

use dashmap::DashMap;
use duct::IntoExecutablePath;
use std::collections::HashMap;
use std::io::Write;

pub fn from_dash<K, V>(input: &DashMap<K, V>) -> HashMap<K, V>
where
    K: std::cmp::Eq + std::hash::Hash + Clone,
    V: Clone,
{
    input
        .iter()
        .map(|x| (x.key().clone(), x.value().clone()))
        .collect()
}

pub fn cmd<P, S, In, A, I>(
    path: P,
    log: &Log,
    program: S,
    args: I,
    input: &mut In,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    In: std::io::Read,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let (pipe_reader, mut pipe_writer) = os_pipe::pipe()?;
    let mut expr = duct::cmd(program, args)
        .dir(path.as_ref())
        .stderr_to_stdout()
        .stdout_file(log)
        .stdin_file(pipe_reader);
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }

    let handle = expr.unchecked().start()?;
    std::io::copy(input, &mut pipe_writer)?;
    pipe_writer.flush()?;
    drop(pipe_writer);
    let output = handle.wait()?;
    Ok(output.status.success())
}

pub fn cmd_collect_out<P, S, A, I>(
    path: P,
    log: &Log,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<Vec<u8>>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args)
        .stdout_capture()
        .stderr_file(log)
        .unchecked()
        .dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.run()?;
    Ok(output.stdout)
}

pub fn cmd_pipeout<P, F, S, A, I>(
    path: P,
    log: &Log,
    out: F,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    F: IntoRawFd,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args)
        .stdout_file(out)
        .stderr_file(log)
        .unchecked()
        .dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.run()?;
    Ok(output.status.success())
}

pub fn cmd_noinput<P, S, A, I>(
    path: P,
    log: &Log,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args)
        .stderr_to_stdout()
        .stdout_file(log)
        .unchecked()
        .dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.run()?;

    Ok(output.status.success())
}

pub fn cmd_noredirect<P, S, A, I>(
    path: P,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args).dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.unchecked().run()?;
    Ok(output.status.success())
}

pub fn cmd_nulled<P, S, A, I>(
    path: P,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args)
        .stdout_null()
        .stderr_null()
        .unchecked()
        .dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.run()?;
    Ok(output.status.success())
}
