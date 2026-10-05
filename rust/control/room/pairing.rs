//! Pairing connectors with the room and authenticating them: the local credential, one-time
//! pairing codes and the list of paired machines.
use std::io;

use serde_json::{json, Value};

use super::util::field;
use super::Room;

impl Room {
    pub fn local_credential(&self) -> io::Result<(String, String)> {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(saved) = self
            .dir
            .read_json("connector-credential.json")
            .ok()
            .flatten()
        {
            let cid = field(&saved, "connector_id");
            let token = field(&saved, "token");
            if inner.credentials.is_paired(cid, token) {
                return Ok((cid.into(), token.into()));
            }
        }
        let (cid, token) = inner.credentials.issue(None);
        self.save(&inner.credentials)?;
        self.dir.write_json(
            "connector-credential.json",
            &json!({"connector_id": cid, "token": token}),
        )?;
        Ok((cid, token))
    }
    pub fn authenticate_connector(&self, cid: &str, token: &str, identity: &Value) -> bool {
        let mut inner = self.inner.lock().expect("room lock");
        if !inner.credentials.is_paired(cid, token) {
            return false;
        }
        inner.credentials.touch(cid, identity);
        self.save(&inner.credentials).is_ok()
    }
    pub fn pairing_code(&self) -> String {
        let code = self
            .inner
            .lock()
            .expect("room lock")
            .credentials
            .new_pairing_code();
        format!("{}-{}-{}", &code[..4], &code[4..8], &code[8..])
    }
    pub fn redeem_pairing(
        &self,
        code: &str,
        identity: &Value,
    ) -> io::Result<Option<(String, String)>> {
        let mut inner = self.inner.lock().expect("room lock");
        if !inner.credentials.redeem_pairing_code(code) {
            return Ok(None);
        }
        let issued = inner.credentials.issue(Some(identity));
        self.save(&inner.credentials)?;
        Ok(Some(issued))
    }
    pub fn paired_connectors(&self) -> Value {
        let inner = self.inner.lock().expect("room lock");
        inner.credentials.views(|cid| inner.peers.contains(cid))
    }
}
