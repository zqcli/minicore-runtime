use serde::Deserialize;
use serde_json::Value;

use crate::workspace_v2::RelativePath;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PathArguments {
    path: RelativePath,
}

impl PathArguments {
    pub(crate) fn path(self) -> RelativePath {
        self.path
    }
}

pub(crate) fn parse_path_args(value: &Value, allow_empty: bool) -> Result<RelativePath, ()> {
    let arguments: PathArguments = serde_json::from_value(value.clone()).map_err(|_| ())?;
    let path = arguments.path();
    validate_path(&path, allow_empty)?;
    Ok(path)
}

pub(crate) fn validate_path(path: &RelativePath, allow_empty: bool) -> Result<(), ()> {
    if !allow_empty && path.is_empty() {
        return Err(());
    }
    Ok(())
}
