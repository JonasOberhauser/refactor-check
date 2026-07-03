use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    Command { name: String, args: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMsg {
    Output { text: String, kind: OutKind },
    Error { text: String },
    Status { state: WorkState, message: String },
    Commands { list: Vec<CommandInfo> },
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OutKind {
    Info,
    Error,
    Output,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkState {
    Running,
    Finished,
    Failed,
}

pub fn read_msg<R: std::io::BufRead, T: serde::de::DeserializeOwned>(r: R) -> std::io::Result<Option<T>> {
    let mut iter = r.lines();
    let line = iter.next().transpose()?;
    match line {
        Some(line) if !line.is_empty() => {
            serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }
        _ => Ok(None),
    }
}

pub fn write_msg<W: std::io::Write, T: serde::Serialize>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let json = serde_json::to_string(msg)?;
    writeln!(w, "{json}")
}
