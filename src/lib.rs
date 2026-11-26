mod discover;
mod evidence;
mod extract;
mod graph;
mod nx;
mod project;
mod react;
mod report;
mod source_map;

use std::path::Path;

use napi::{Env, Error, Result, Task, bindgen_prelude::AsyncTask};
use napi_derive::napi;
use report::Report;

#[napi]
pub struct Snapshot {
    report: Option<Report>,
}
impl Snapshot {
    fn live(&self) -> Result<&Report> {
        self.report
            .as_ref()
            .ok_or_else(|| Error::from_reason("Disposed analysis report."))
    }
}
#[napi]
impl Snapshot {
    #[napi]
    pub fn metadata(&self) -> Result<String> {
        self.live()?
            .metadata()
            .map_err(|e| Error::from_reason(e.to_string()))
    }
    #[napi]
    pub fn findings(&self, kind: Option<String>) -> Result<String> {
        self.live()?
            .findings(kind.as_deref())
            .map_err(|e| Error::from_reason(e.to_string()))
    }
    #[napi]
    pub fn explain(&self, selector: String) -> Result<String> {
        let value =
            serde_json::from_str(&selector).map_err(|e| Error::from_reason(e.to_string()))?;
        self.live()?
            .explain(&value)
            .map(|v| v.to_string())
            .map_err(Error::from_reason)
    }
    #[napi]
    pub fn dispose(&mut self) {
        self.report.take();
    }
}

pub struct Analysis {
    root: String,
    options: String,
}

impl Task for Analysis {
    type Output = Report;
    type JsValue = Snapshot;
    fn compute(&mut self) -> Result<Report> {
        let config =
            serde_json::from_str(&self.options).map_err(|e| Error::from_reason(e.to_string()))?;
        Report::build(Path::new(&self.root), config).map_err(Error::from_reason)
    }
    fn resolve(&mut self, _: Env, report: Report) -> Result<Snapshot> {
        Ok(Snapshot {
            report: Some(report),
        })
    }
}

#[napi]
pub fn analyze_native(root: String, options: String) -> AsyncTask<Analysis> {
    AsyncTask::new(Analysis { root, options })
}
