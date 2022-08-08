use std::{collections::HashMap, io, path::PathBuf, process::Stdio};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{ChildStdin, ChildStdout, Command},
};

use crate::uci::{UciIn, UciOption, UciOptionName, UciOut};

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct Session(pub u64);

pub struct Engine {
    pending_uciok: u64,
    pending_readyok: u64,
    searching: bool,
    options: HashMap<UciOptionName, UciOption>,
    name: Option<String>,
    params: EngineParameters,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

pub struct EngineParameters {
    pub max_threads: u32,
    pub max_hash: u32,
}

impl Engine {
    pub async fn new(
        path: PathBuf,
        params: EngineParameters,
        options: HashMap<UciOptionName, String>,
    ) -> io::Result<Engine> {
        log::info!("Starting engine {path:?} ...");

        let mut process = Command::new(path)
            .stdout(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()?;

        let mut engine =
            Engine {
                pending_uciok: 0,
                pending_readyok: 0,
                searching: false,
                options: HashMap::new(),
                name: None,
                params,
                stdin: BufWriter::new(process.stdin.take().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "engine stdin closed")
                })?),
                stdout: BufReader::new(process.stdout.take().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "engine stdout closed")
                })?),
            };

        let session = Session(0);
        engine.send(session, UciIn::Uci).await?;
        engine.ensure_idle(session).await?;
        for (name, value) in options {
            engine
                .send_dangerous(
                    session,
                    UciIn::Setoption {
                        name,
                        value: Some(value),
                    },
                )
                .await?;
        }
        Ok(engine)
    }

    pub async fn send(&mut self, session: Session, command: UciIn) -> io::Result<()> {
        match command {
            UciIn::Setoption { ref name, .. } if !name.is_safe() => {
                log::error!(
                    "{}: rejected potentially unsafe option: {}",
                    session.0,
                    command
                );
                Ok(())
            }
            _ => self.send_dangerous(session, command).await,
        }
    }

    pub async fn send_dangerous(&mut self, session: Session, command: UciIn) -> io::Result<()> {
        match command {
            UciIn::Isready => self.pending_readyok += 1,
            UciIn::Stop | UciIn::Ponderhit => (),
            _ if self.searching => {
                log::error!("{}: engine is busy: {}", session.0, command);
                return Err(io::Error::new(io::ErrorKind::Other, "engine is busy"));
            }
            UciIn::Uci => {
                self.pending_uciok += 1;
                self.options.clear();
                self.name.take();
            }
            UciIn::Go { .. } => {
                self.searching = true;
            }
            UciIn::Setoption {
                ref name,
                ref value,
            } => match self.options.get(name) {
                Some(option) => {
                    option
                        .validate(value.clone())
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                }
                None => {
                    log::warn!("{}: ignoring unknown option: {}", session.0, command);
                    return Ok(());
                }
            },
            _ => (),
        }

        let mut buf = command.to_string();
        log::info!("{} << {}", session.0, buf);
        buf.push_str("\r\n");
        self.stdin.write_all(buf.as_bytes()).await?;
        self.stdin.flush().await
    }

    pub async fn recv(&mut self, session: Session) -> io::Result<UciOut> {
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let line = line.trim_end_matches(|c| c == '\r' || c == '\n');

            let mut command = match UciOut::from_line(line) {
                Err(err) => {
                    log::error!("{} >> {}", session.0, line);
                    return Err(io::Error::new(io::ErrorKind::InvalidData, err));
                }
                Ok(None) => {
                    log::warn!("{} >> {}", session.0, line);
                    continue;
                }
                Ok(Some(command)) => command,
            };

            match command {
                UciOut::Info {
                    pv: None,
                    string: None,
                    score: None,
                    ..
                } => {
                    // Skip noise.
                    log::trace!("{} >> {}", session.0, command);
                    continue;
                }
                UciOut::Info { .. } => log::debug!("{} >> {}", session.0, command),
                _ => log::info!("{} >> {}", session.0, command),
            }

            match command {
                UciOut::IdName(ref name) => self.name = Some(name.clone()),
                UciOut::Uciok => self.pending_uciok = self.pending_uciok.saturating_sub(1),
                UciOut::Readyok => self.pending_readyok = self.pending_readyok.saturating_sub(1),
                UciOut::Bestmove { .. } => self.searching = false,
                UciOut::Option {
                    ref name,
                    ref mut option,
                } => {
                    // Apply limits set in engine parameters.
                    if *name == "Threads" {
                        option.limit_max(self.params.max_threads.into());
                    } else if *name == "Hash" {
                        option.limit_max(self.params.max_hash.into());
                    }

                    self.options.insert(name.clone(), option.clone());
                }
                _ => (),
            }

            return Ok(command);
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn max_threads(&self) -> i64 {
        self.options
            .get(&UciOptionName("Threads".to_owned()))
            .and_then(UciOption::max)
            .unwrap_or(1)
    }

    pub fn max_hash(&self) -> i64 {
        self.options
            .get(&UciOptionName("Hash".to_owned()))
            .and_then(UciOption::max)
            .unwrap_or(16)
    }

    pub fn variants(&self) -> &[String] {
        self.options
            .get(&UciOptionName("UCI_Variant".to_owned()))
            .and_then(UciOption::var)
            .unwrap_or_default()
    }

    pub fn is_searching(&self) -> bool {
        self.searching
    }

    pub fn is_idle(&self) -> bool {
        self.pending_uciok == 0 && self.pending_readyok == 0 && !self.searching
    }

    pub async fn ensure_idle(&mut self, session: Session) -> io::Result<()> {
        while !self.is_idle() {
            if self.searching && self.pending_readyok < 1 {
                self.send(session, UciIn::Stop).await?;
                self.send(session, UciIn::Isready).await?;
            }
            self.recv(session).await?;
        }
        Ok(())
    }

    pub async fn ensure_newgame(&mut self, session: Session) -> io::Result<()> {
        self.ensure_idle(session).await?;
        self.send(session, UciIn::Ucinewgame).await?;
        self.send(session, UciIn::Isready).await?;
        self.ensure_idle(session).await?;
        Ok(())
    }
}
