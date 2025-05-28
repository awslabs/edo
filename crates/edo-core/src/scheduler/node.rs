use crate::context::Addr;
use std::sync::atomic::{AtomicU8, Ordering};

pub struct Node {
    pub addr: Addr,
    pub status: AtomicU8,
}

impl Node {
    pub fn new(addr: &Addr) -> Self {
        Self {
            addr: addr.clone(),
            status: AtomicU8::new(NodeStatus::Pending as u8),
        }
    }

    pub fn set_queued(&self) {
        self.status
            .store(NodeStatus::Queued as u8, Ordering::SeqCst);
    }

    pub fn set_running(&self) {
        self.status
            .store(NodeStatus::Running as u8, Ordering::SeqCst);
    }

    pub fn set_failed(&self) {
        self.status
            .store(NodeStatus::Failed as u8, Ordering::SeqCst);
    }

    pub fn set_success(&self) {
        self.status
            .store(NodeStatus::Success as u8, Ordering::SeqCst);
    }

    pub fn is_pending(&self) -> bool {
        let status = self.status();
        status == NodeStatus::Pending
    }

    pub fn is_queued(&self) -> bool {
        let status = self.status();
        status == NodeStatus::Queued
    }

    pub fn is_failed(&self) -> bool {
        let status = self.status();
        status == NodeStatus::Failed
    }

    pub fn is_done(&self) -> bool {
        let status = self.status();
        status == NodeStatus::Success || status == NodeStatus::Failed
    }

    pub fn status(&self) -> NodeStatus {
        NodeStatus::from(self.status.load(Ordering::SeqCst))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum NodeStatus {
    Pending = 0,
    Queued = 1,
    Running = 2,
    Failed = 3,
    Success = 4,
}

impl From<u8> for NodeStatus {
    fn from(value: u8) -> Self {
        match value {
            x if x == NodeStatus::Pending as u8 => NodeStatus::Pending,
            x if x == NodeStatus::Queued as u8 => NodeStatus::Queued,
            x if x == NodeStatus::Running as u8 => NodeStatus::Running,
            x if x == NodeStatus::Failed as u8 => NodeStatus::Failed,
            x if x == NodeStatus::Success as u8 => NodeStatus::Success,
            _ => NodeStatus::Failed,
        }
    }
}
