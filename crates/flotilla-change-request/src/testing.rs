use crate::ChangeRequestTracker;
use async_trait::async_trait;
use flotilla_protocol::ChangeRequest;

/// A read-only tracker over a fixed listing; exercises the trait's defaults.
pub struct ListedChangeRequests(pub Vec<(String, ChangeRequest)>);

#[async_trait]
impl ChangeRequestTracker for ListedChangeRequests {
    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        Ok(self.0.iter().take(limit).cloned().collect())
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        self.0.iter().find(|(request_id, _)| request_id == id).cloned().ok_or_else(|| format!("change request {id} not found"))
    }

    async fn open_in_browser(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn close_change_request(&self, _id: &str) -> Result<(), String> {
        Err("read-only test tracker".into())
    }

    async fn merge_change_request(&self, _id: &str) -> Result<(), String> {
        Err("read-only test tracker".into())
    }

    async fn list_merged_branch_names(&self, _limit: usize) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}
