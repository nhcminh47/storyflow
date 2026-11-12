use crate::{
    Result,
    model::{Event, State},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use storyflow::snapshot::RunSnapshot;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    schema: u32,
    revision: usize,
    previous_sha256: Option<String>,
    unix_ms: u64,
    pub event: Event,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    body: Body,
    sha256: String,
}

impl Body {
    fn checksum(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}

pub struct Journal {
    pub dir: PathBuf,
    id: String,
    pub records: Vec<Record>,
    pub state: Option<State>,
    pub snapshot: Option<RunSnapshot>,
    _lock: File,
}

impl Journal {
    pub fn open(root: &Path, id: &str, init: bool) -> Result<Self> {
        if id.is_empty()
            || id.len() > 40
            || !id.as_bytes()[0].is_ascii_lowercase()
            || !id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            || id.ends_with('-')
            || matches!(
                id,
                "con"
                    | "prn"
                    | "aux"
                    | "nul"
                    | "com1"
                    | "com2"
                    | "com3"
                    | "com4"
                    | "com5"
                    | "com6"
                    | "com7"
                    | "com8"
                    | "com9"
                    | "lpt1"
                    | "lpt2"
                    | "lpt3"
                    | "lpt4"
                    | "lpt5"
                    | "lpt6"
                    | "lpt7"
                    | "lpt8"
                    | "lpt9"
            )
        {
            return Err("invalid story id; use lowercase ASCII letters, digits and hyphens".into());
        }
        let dir = root.join(".storyflow").join(id);
        let events = dir.join("events");
        if init {
            fs::create_dir_all(&events)?;
        } else if !events.is_dir() {
            return Err(format!("run {id} has not been initialized").into());
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("run.lock"))?;
        lock.try_lock()
            .map_err(|error| format!("cannot lock run {id}: {error}"))?;
        let mut journal = Self {
            dir,
            id: id.into(),
            records: Vec::new(),
            state: None,
            snapshot: None,
            _lock: lock,
        };
        let mut paths = fs::read_dir(events)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        for path in paths {
            if path.file_name().and_then(|name| name.to_str()) == Some("pending.tmp") {
                continue;
            }
            let revision = journal.records.len();
            if path != journal.event_path(revision) {
                return Err(format!("unexpected journal entry: {}", path.display()).into());
            }
            let record: Record = serde_json::from_slice(&fs::read(path)?)?;
            if record.body.schema != 1
                || record.body.revision != revision
                || record.body.previous_sha256.as_ref() != journal.last_checksum()
                || record.sha256 != record.body.checksum()?
            {
                return Err(
                    format!("journal integrity check failed at revision {revision}").into(),
                );
            }
            journal.apply(&record.body.event)?;
            journal.records.push(record);
        }
        if let Some(state) = &journal.state
            && let Some(expected) = &state.config_sha256
        {
            let bytes = fs::read(journal.dir.join("config-snapshot.json")).map_err(
                |_| "missing_config_snapshot: restore the original snapshot; rebind is unsupported",
            )?;
            let snapshot: RunSnapshot = serde_json::from_slice(&bytes).map_err(
                |_| "invalid_config_snapshot: restore the original snapshot; rebind is unsupported",
            )?;
            snapshot.verify(id, &state.base, expected)?;
            journal.snapshot = Some(snapshot);
        }
        Ok(journal)
    }

    /// Only an empty run can bind inputs. Active runs cannot migrate or rebind.
    pub fn pin_snapshot(&mut self, snapshot: RunSnapshot) -> Result<String> {
        if self.state.is_some() {
            return Err(
                "config_rebind_unsupported: initialize a new story run on an approved baseline"
                    .into(),
            );
        }
        let digest = snapshot.digest()?;
        snapshot.verify(&self.id, &snapshot.base, &digest)?;
        let pending = self.dir.join("config-snapshot.pending.tmp");
        {
            let mut file = File::create(&pending)?;
            serde_json::to_writer_pretty(&mut file, &snapshot)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        fs::rename(pending, self.dir.join("config-snapshot.json"))?;
        self.snapshot = Some(snapshot);
        Ok(digest)
    }

    pub fn state(&self) -> Result<&State> {
        self.state
            .as_ref()
            .ok_or_else(|| "run has no init event".into())
    }

    pub fn revision(&self) -> Result<usize> {
        Ok(self
            .records
            .len()
            .checked_sub(1)
            .ok_or("run has no revision")?)
    }

    pub fn expect_revision(&self, revision: &str) -> Result<()> {
        let current = self.revision()?;
        if revision.parse::<usize>()? != current {
            return Err(format!("stale revision; current revision is {current}; run next").into());
        }
        Ok(())
    }

    pub fn append(&mut self, event: Event) -> Result<usize> {
        if let Event::Init {
            story,
            base,
            config_sha256: Some(expected),
        } = &event
        {
            self.snapshot
                .as_ref()
                .ok_or("missing_config_snapshot")?
                .verify(story, base, expected)?;
        }
        // Validate a copy, publish memory only after the durable event has been renamed.
        let mut next_state = self.state.clone();
        if let Some(state) = &mut next_state {
            state.apply(&event)?;
        } else {
            next_state = Some(State::new(&event, &self.id)?);
        }
        let revision = self.records.len();
        let body = Body {
            schema: 1,
            revision,
            previous_sha256: self.last_checksum().cloned(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_millis()
                .try_into()?,
            event,
        };
        let record = Record {
            sha256: body.checksum()?,
            body,
        };
        let path = self.event_path(revision);
        let pending = path.with_file_name("pending.tmp");
        {
            let mut file = File::create(&pending)?;
            serde_json::to_writer_pretty(&mut file, &record)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        fs::rename(pending, path)?;
        self.state = next_state;
        self.records.push(record);
        Ok(revision)
    }

    fn apply(&mut self, event: &Event) -> Result<()> {
        if let Some(state) = &mut self.state {
            state.apply(event)
        } else {
            self.state = Some(State::new(event, &self.id)?);
            Ok(())
        }
    }

    fn last_checksum(&self) -> Option<&String> {
        self.records.last().map(|record| &record.sha256)
    }

    fn event_path(&self, revision: usize) -> PathBuf {
        self.dir.join("events").join(format!("{revision:012}.json"))
    }
}
