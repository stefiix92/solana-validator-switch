use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Terminal,
};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;

use crate::alert::{AlertManager, ComprehensiveAlertTracker};
use crate::solana_rpc::{fetch_vote_account_data, ValidatorVoteData};
use crate::types::{FailureTracker, NodeHealthStatus};
use crate::{ssh::AsyncSshPool, AppState};

/// View states for the UI
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewState {
    Status,
    Switch,
}

/// Enhanced UI App state with async support
pub struct EnhancedStatusApp {
    pub app_state: Arc<AppState>,
    pub ssh_pool: Arc<AsyncSshPool>,
    pub ui_state: Arc<RwLock<UiState>>,
    pub log_sender: tokio::sync::mpsc::UnboundedSender<LogMessage>,
    pub should_quit: Arc<RwLock<bool>>,
    pub view_state: Arc<RwLock<ViewState>>,
    pub emergency_takeover_in_progress: Arc<RwLock<bool>>,
    pub switch_confirmed: Arc<RwLock<bool>>,
}

/// UI State that can be shared across threads
pub struct UiState {
    // Vote data for each validator
    pub vote_data: Vec<Option<ValidatorVoteData>>,
    pub previous_last_slots: Vec<Option<u64>>,
    pub increment_times: Vec<Option<Instant>>,

    // Track when each validator's last vote slot changed
    pub last_vote_slot_times: Vec<Option<(u64, Instant)>>, // (slot, time when slot last changed)

    // Catchup status for each node
    pub catchup_data: Vec<NodePairStatus>,

    // Track consecutive catchup failures for standby nodes
    #[allow(dead_code)]
    pub catchup_failure_counts: Vec<(u32, u32)>, // (node_0_failures, node_1_failures)
    
    // Track last alert time for catchup failures
    #[allow(dead_code)]
    pub last_catchup_alert_times: Vec<(Option<Instant>, Option<Instant>)>, // (node_0_last_alert, node_1_last_alert)

    // SSH health status for each node
    pub ssh_health_data: Vec<NodePairSshStatus>,

    // Comprehensive health tracking for each validator
    pub validator_health: Vec<NodeHealthStatus>,
    
    // RPC failure tracking for each validator
    pub rpc_failure_tracker: Vec<FailureTracker>,

    // Refresh state
    pub last_vote_refresh: Instant,
    pub last_catchup_refresh: Instant,
    pub last_ssh_health_refresh: Instant,
    
    // Field refresh states - tracks which fields are being refreshed for each validator/node
    pub field_refresh_states: Vec<NodeFieldRefreshState>,
    
    // Refreshed validator statuses - stores the latest refreshed data
    pub validator_statuses: Vec<crate::ValidatorStatus>,
    
    #[allow(dead_code)]
    pub is_refreshing: bool,
}

#[derive(Debug, Clone)]
pub struct NodeFieldRefreshState {
    pub node_0: FieldRefreshStates,
    pub node_1: FieldRefreshStates,
}

#[derive(Debug, Clone)]
pub struct FieldRefreshStates {
    pub status_refreshing: bool,
    pub identity_refreshing: bool,
    pub version_refreshing: bool,
    pub catchup_refreshing: bool,
    pub health_refreshing: bool,
}

impl Default for FieldRefreshStates {
    fn default() -> Self {
        Self {
            status_refreshing: false,
            identity_refreshing: false,
            version_refreshing: false,
            catchup_refreshing: false,
            health_refreshing: false,
        }
    }
}

// Removed FocusedPane enum as logs are no longer displayed

#[derive(Clone)]
pub struct NodePairStatus {
    pub node_0: Option<CatchupStatus>,
    pub node_1: Option<CatchupStatus>,
}

#[derive(Clone)]
pub struct CatchupStatus {
    pub status: String,
    #[allow(dead_code)]
    pub last_updated: Instant,
    pub is_streaming: bool,
}

#[derive(Clone)]
pub struct NodePairSshStatus {
    pub node_0: SshHealthStatus,
    pub node_1: SshHealthStatus,
}

#[derive(Clone)]
pub struct SshHealthStatus {
    pub is_healthy: bool,
    pub last_success: Option<Instant>,
    pub failure_start: Option<Instant>,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct LogMessage {
    pub host: String,
    pub message: String,
    pub timestamp: Instant,
    pub level: LogLevel,
}

#[derive(Clone, Copy)]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

impl EnhancedStatusApp {
    pub async fn new(app_state: Arc<AppState>) -> Result<Self> {
        let ssh_pool = Arc::clone(&app_state.ssh_pool);

        // Create unbounded channel for log messages
        let (log_sender, _log_receiver) = tokio::sync::mpsc::unbounded_channel();

        // Initialize UI state
        let mut initial_vote_data = Vec::new();
        let mut initial_catchup_data = Vec::new();
        let mut initial_ssh_health_data = Vec::new();

        for validator_status in &app_state.validator_statuses {
            initial_vote_data.push(None);

            // Initialize catchup status for standby nodes
            let mut node_pair = NodePairStatus {
                node_0: None,
                node_1: None,
            };
            
            if validator_status.nodes_with_status.len() >= 2 {
                // Initialize for standby nodes or Firedancer nodes
                if validator_status.nodes_with_status[0].status == crate::types::NodeStatus::Standby 
                    || validator_status.nodes_with_status[0].validator_type == crate::types::ValidatorType::Firedancer {
                    node_pair.node_0 = Some(CatchupStatus {
                        status: "⏳ Initializing...".to_string(),
                        last_updated: Instant::now(),
                        is_streaming: false,
                    });
                }
                if validator_status.nodes_with_status[1].status == crate::types::NodeStatus::Standby 
                    || validator_status.nodes_with_status[1].validator_type == crate::types::ValidatorType::Firedancer {
                    node_pair.node_1 = Some(CatchupStatus {
                        status: "⏳ Initializing...".to_string(),
                        last_updated: Instant::now(),
                        is_streaming: false,
                    });
                }
            }
            
            initial_catchup_data.push(node_pair);

            let ssh_pair = NodePairSshStatus {
                node_0: SshHealthStatus {
                    is_healthy: true,
                    last_success: Some(Instant::now()),
                    failure_start: None,
                },
                node_1: SshHealthStatus {
                    is_healthy: true,
                    last_success: Some(Instant::now()),
                    failure_start: None,
                },
            };
            initial_ssh_health_data.push(ssh_pair);
        }

        // Initialize health tracking
        let mut initial_validator_health = Vec::new();
        let mut initial_rpc_trackers = Vec::new();
        for _ in 0..app_state.validator_statuses.len() {
            initial_validator_health.push(NodeHealthStatus {
                ssh_status: FailureTracker::new(),
                rpc_status: FailureTracker::new(),
                is_voting: true,
                last_vote_slot: None,
                last_vote_time: None,
            });
            initial_rpc_trackers.push(FailureTracker::new());
        }

        // Initialize field refresh states
        let initial_field_refresh_states = (0..app_state.validator_statuses.len())
            .map(|_| NodeFieldRefreshState {
                node_0: FieldRefreshStates::default(),
                node_1: FieldRefreshStates::default(),
            })
            .collect();

        let ui_state = Arc::new(RwLock::new(UiState {
            vote_data: initial_vote_data,
            previous_last_slots: Vec::new(),
            increment_times: Vec::new(),
            last_vote_slot_times: vec![None; app_state.validator_statuses.len()],
            catchup_data: initial_catchup_data,
            catchup_failure_counts: vec![(0, 0); app_state.validator_statuses.len()],
            last_catchup_alert_times: vec![(None, None); app_state.validator_statuses.len()],
            ssh_health_data: initial_ssh_health_data,
            validator_health: initial_validator_health,
            rpc_failure_tracker: initial_rpc_trackers,
            last_vote_refresh: Instant::now(),
            last_catchup_refresh: Instant::now(),
            last_ssh_health_refresh: Instant::now(),
            field_refresh_states: initial_field_refresh_states,
            validator_statuses: app_state.validator_statuses.clone(),
            is_refreshing: false,
        }));

        Ok(Self {
            app_state,
            ssh_pool,
            ui_state,
            log_sender,
            should_quit: Arc::new(RwLock::new(false)),
            view_state: Arc::new(RwLock::new(ViewState::Status)),
            emergency_takeover_in_progress: Arc::new(RwLock::new(false)),
            switch_confirmed: Arc::new(RwLock::new(false)),
        })
    }
    
    /// Spawn continuous catchup streaming tasks for each node
    fn spawn_catchup_streaming_tasks(&self) {
        let ui_state = Arc::clone(&self.ui_state);
        let app_state = Arc::clone(&self.app_state);
        let ssh_pool = Arc::clone(&self.ssh_pool);
        let log_sender = self.log_sender.clone();
        
        // Spawn a streaming task for each node
        for (validator_idx, validator_status) in app_state.validator_statuses.iter().enumerate() {
            for (node_idx, node) in validator_status.nodes_with_status.iter().enumerate() {
                let node = node.clone();
                let ui_state = Arc::clone(&ui_state);
                let ssh_pool = Arc::clone(&ssh_pool);
                let log_sender = log_sender.clone();
                let ssh_key = app_state.detected_ssh_keys.get(&node.node.host).cloned();
                
                if let Some(ssh_key) = ssh_key {
                    tokio::spawn(async move {
                        stream_catchup_for_node(
                            ssh_pool,
                            node,
                            ssh_key,
                            ui_state,
                            validator_idx,
                            node_idx,
                            log_sender,
                        ).await;
                    });
                }
            }
        }
    }

    /// Spawn background tasks for data fetching
    pub fn spawn_background_tasks(&self) {
        // Spawn continuous catchup streaming tasks for each node
        self.spawn_catchup_streaming_tasks();
        
        // Vote data refresh task
        let ui_state = Arc::clone(&self.ui_state);
        let app_state = Arc::clone(&self.app_state);
        let log_sender = self.log_sender.clone();
        let emergency_takeover_flag = Arc::clone(&self.emergency_takeover_in_progress);

        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(5));

            // Initialize alert manager and tracker if alerts are configured
            let alert_manager = app_state
                .config
                .alert_config
                .as_ref()
                .filter(|config| config.enabled)
                .map(|config| AlertManager::new(config.clone()));

            let nodes_per_validator = 2; // Assuming 2 nodes per validator
            let mut alert_tracker = ComprehensiveAlertTracker::new(
                app_state.validator_statuses.len(),
                nodes_per_validator
            );

            loop {
                interval.tick().await;

                // Fetch vote data for all validators
                let mut new_vote_data = Vec::new();

                for (idx, validator_status) in app_state.validator_statuses.iter().enumerate() {
                    let validator_pair = &validator_status.validator_pair;

                    match fetch_vote_account_data(&validator_pair.rpc, &validator_pair.vote_pubkey)
                        .await
                    {
                        Ok(data) => {
                            // Update RPC success
                            {
                                let mut state = ui_state.write().await;
                                state.rpc_failure_tracker[idx].record_success();
                            }

                            let _ = log_sender.send(LogMessage {
                                host: format!("validator-{}", idx),
                                message: format!(
                                    "Vote data fetched: last slot {}",
                                    data.recent_votes.last().map(|v| v.slot).unwrap_or(0)
                                ),
                                timestamp: Instant::now(),
                                level: LogLevel::Info,
                            });

                            new_vote_data.push(Some(data));
                        }
                        Err(e) => {
                            // Update RPC failure
                            let (should_alert_rpc, consecutive_failures, seconds_since_first) = {
                                let mut state = ui_state.write().await;
                                state.rpc_failure_tracker[idx].record_failure(e.to_string());
                                
                                let tracker = &state.rpc_failure_tracker[idx];
                                let consecutive = tracker.consecutive_failures;
                                let seconds = tracker.seconds_since_first_failure().unwrap_or(0);
                                
                                let config = app_state.config.alert_config.as_ref();
                                let time_threshold = config.map(|c| c.rpc_failure_threshold_seconds).unwrap_or(1800);
                                
                                let should_alert = seconds >= time_threshold
                                    && alert_tracker.rpc_failure_tracker.should_send_alert(idx);
                                
                                (should_alert, consecutive, seconds)
                            };

                            // Send RPC failure alert if needed
                            if should_alert_rpc {
                                if let Some(alert_mgr) = alert_manager.as_ref() {
                                    let _ = alert_mgr.send_rpc_failure_alert(
                                        &validator_pair.identity_pubkey,
                                        &validator_pair.vote_pubkey,
                                        consecutive_failures,
                                        seconds_since_first,
                                        &e.to_string()
                                    ).await;
                                }
                            }

                            let _ = log_sender.send(LogMessage {
                                host: format!("validator-{}", idx),
                                message: format!("Failed to fetch vote data: {}", e),
                                timestamp: Instant::now(),
                                level: LogLevel::Error,
                            });

                            new_vote_data.push(None);
                        }
                    }
                }

                // Update UI state
                let mut state = ui_state.write().await;

                // Calculate increments and track slot changes
                let mut new_increments = Vec::new();
                let mut new_slot_times = Vec::new();

                for (idx, new_data) in new_vote_data.iter().enumerate() {
                    if let Some(new) = new_data {
                        let new_last_slot = new.recent_votes.last().map(|v| v.slot);

                        // Check if this is a new slot
                        if let Some(new_slot) = new_last_slot {
                            // Check against our tracked slot time
                            let should_update_slot_time = if let Some(tracked) =
                                state.last_vote_slot_times.get(idx).and_then(|&v| v)
                            {
                                tracked.0 != new_slot // Slot has changed
                            } else {
                                true // No previous tracking
                            };

                            if should_update_slot_time {
                                new_slot_times.push(Some((new_slot, Instant::now())));
                                // Reset alert tracker since slot is advancing
                                alert_tracker.delinquency_tracker.reset(idx);
                            } else {
                                // Slot hasn't changed, keep existing time
                                new_slot_times
                                    .push(state.last_vote_slot_times.get(idx).and_then(|&v| v));

                                // Check for delinquency
                                if let (Some(alert_mgr), Some((_, last_change_time))) = (
                                    alert_manager.as_ref(),
                                    state.last_vote_slot_times.get(idx).and_then(|&v| v),
                                ) {
                                    let seconds_since_vote = last_change_time.elapsed().as_secs();
                                    let threshold = app_state
                                        .config
                                        .alert_config
                                        .as_ref()
                                        .map(|c| c.delinquency_threshold_seconds)
                                        .unwrap_or(30);

                                    if seconds_since_vote >= threshold
                                        && alert_tracker.delinquency_tracker.should_send_alert(idx)
                                    {
                                        // Find which node is active
                                        let active_node = if let Some(node_with_status) = app_state
                                            .validator_statuses[idx]
                                            .nodes_with_status
                                            .iter()
                                            .find(|n| n.status == crate::types::NodeStatus::Active)
                                        {
                                            &node_with_status.node
                                        } else {
                                            &app_state.validator_statuses[idx].nodes_with_status[0]
                                                .node
                                        };

                                        let is_active = app_state.validator_statuses[idx]
                                            .nodes_with_status
                                            .iter()
                                            .any(|n| n.status == crate::types::NodeStatus::Active);

                                        // Get current health status
                                        let node_health = state.validator_health[idx].clone();
                                        
                                        // Send alert with health status
                                        if let Err(e) = alert_mgr
                                            .send_delinquency_alert_with_health(
                                                &app_state.validator_statuses[idx]
                                                    .validator_pair
                                                    .identity_pubkey,
                                                &active_node.label,
                                                is_active,
                                                new_slot,
                                                seconds_since_vote,
                                                &node_health,
                                            )
                                            .await
                                        {
                                            let _ = log_sender.send(LogMessage {
                                                host: format!("validator-{}", idx),
                                                message: format!(
                                                    "Failed to send delinquency alert: {}",
                                                    e
                                                ),
                                                timestamp: Instant::now(),
                                                level: LogLevel::Error,
                                            });
                                        } else {
                                            let _ = log_sender.send(LogMessage {
                                                host: format!("validator-{}", idx),
                                                message: format!("Delinquency alert sent: {} seconds without vote", seconds_since_vote),
                                                timestamp: Instant::now(),
                                                level: LogLevel::Warning,
                                            });
                                        }
                                        
                                        // Check if auto-failover is enabled
                                        if let Some(alert_config) = &app_state.config.alert_config {
                                            if alert_config.enabled && alert_config.auto_failover_enabled {
                                                // CRITICAL: Only trigger auto-failover if RPC is working
                                                // We need RPC to verify on-chain that the validator is not voting
                                                // SSH may be down if the node is completely offline
                                                if node_health.rpc_status.consecutive_failures == 0 {
                                                    
                                                    let _ = log_sender.send(LogMessage {
                                                        host: format!("validator-{}", idx),
                                                        message: "🚨 AUTO-FAILOVER: Initiating emergency takeover".to_string(),
                                                        timestamp: Instant::now(),
                                                        level: LogLevel::Error,
                                                    });
                                                    
                                                    // Spawn emergency failover task
                                                    let validator_status = app_state.validator_statuses[idx].clone();
                                                    let alert_manager = alert_mgr.clone();
                                                    let ssh_pool = app_state.ssh_pool.clone();
                                                    let ssh_keys = app_state.detected_ssh_keys.clone();
                                                    let emergency_flag = emergency_takeover_flag.clone();
                                                    
                                                    tokio::spawn(async move {
                                                        execute_emergency_failover(
                                                            validator_status,
                                                            alert_manager,
                                                            ssh_pool,
                                                            ssh_keys,
                                                            emergency_flag,
                                                        ).await;
                                                    });
                                                } else {
                                                    let _ = log_sender.send(LogMessage {
                                                        host: format!("validator-{}", idx),
                                                        message: format!(
                                                            "Auto-failover suppressed: SSH failures={}, RPC failures={}",
                                                            node_health.ssh_status.consecutive_failures,
                                                            node_health.rpc_status.consecutive_failures
                                                        ),
                                                        timestamp: Instant::now(),
                                                        level: LogLevel::Warning,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Handle increment display (visual indicator)
                            if let Some(old) = state.vote_data.get(idx).and_then(|v| v.as_ref()) {
                                if let Some(old_last_slot) = old.recent_votes.last().map(|v| v.slot)
                                {
                                    if new_slot > old_last_slot {
                                        new_increments.push(Some(Instant::now()));
                                    } else {
                                        // Keep existing increment if still valid
                                        if let Some(existing) =
                                            state.increment_times.get(idx).and_then(|&v| v)
                                        {
                                            if existing.elapsed().as_secs() < 2 {
                                                new_increments.push(Some(existing));
                                            } else {
                                                new_increments.push(None);
                                            }
                                        } else {
                                            new_increments.push(None);
                                        }
                                    }
                                } else {
                                    new_increments.push(None);
                                }
                            } else {
                                new_increments.push(None);
                            }
                        } else {
                            new_increments.push(None);
                            new_slot_times.push(None);
                        }
                    } else {
                        // RPC failed - preserve existing slot time instead of setting to None
                        new_increments.push(None);
                        new_slot_times.push(
                            state.last_vote_slot_times.get(idx).and_then(|&v| v)
                        );
                    }
                }

                // Update previous slots
                state.previous_last_slots = state
                    .vote_data
                    .iter()
                    .map(|v| {
                        v.as_ref()
                            .and_then(|d| d.recent_votes.last().map(|v| v.slot))
                    })
                    .collect();

                state.vote_data = new_vote_data;
                state.increment_times = new_increments;
                state.last_vote_slot_times = new_slot_times;
                state.last_vote_refresh = Instant::now();
            }
        });

        // Catchup status refresh task - DISABLED, using streaming instead
        /*
        let ui_state = Arc::clone(&self.ui_state);
        let app_state = Arc::clone(&self.app_state);
        let ssh_pool = Arc::clone(&self.ssh_pool);
        let log_sender = self.log_sender.clone();

        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(30));

            // Initialize alert manager for catchup failure alerts
            let alert_manager = app_state
                .config
                .alert_config
                .as_ref()
                .filter(|config| config.enabled)
                .map(|config| AlertManager::new(config.clone()));

            loop {
                interval.tick().await;

                // First, set all catchup statuses to "Checking..." to show spinner
                {
                    let mut state = ui_state.write().await;
                    for catchup in &mut state.catchup_data {
                        if catchup.node_0.is_some() {
                            catchup.node_0 = Some(CatchupStatus {
                                status: "Checking...".to_string(),
                                last_updated: Instant::now(),
                                is_streaming: false,
                            });
                        }
                        if catchup.node_1.is_some() {
                            catchup.node_1 = Some(CatchupStatus {
                                status: "Checking...".to_string(),
                                last_updated: Instant::now(),
                                is_streaming: false,
                            });
                        }
                    }
                }

                // Fetch catchup status for all nodes
                let mut new_catchup_data = Vec::new();

                for validator_status in &app_state.validator_statuses {
                    let mut node_pair = NodePairStatus {
                        node_0: None,
                        node_1: None,
                    };

                    if validator_status.nodes_with_status.len() >= 2 {
                        // Fetch for node 0
                        let node_0 = &validator_status.nodes_with_status[0];
                        if let Some(ssh_key) = app_state.detected_ssh_keys.get(&node_0.node.host) {
                            node_pair.node_0 =
                                fetch_catchup_for_node(&ssh_pool, &node_0, ssh_key, &log_sender)
                                    .await;
                        }

                        // Fetch for node 1
                        let node_1 = &validator_status.nodes_with_status[1];
                        if let Some(ssh_key) = app_state.detected_ssh_keys.get(&node_1.node.host) {
                            node_pair.node_1 =
                                fetch_catchup_for_node(&ssh_pool, &node_1, ssh_key, &log_sender)
                                    .await;
                        }
                    }

                    new_catchup_data.push(node_pair);
                }

                // Process failure tracking and alerts
                let mut alerts_to_send = Vec::new();
                let mut failure_updates = Vec::new();
                
                for (idx, new_pair) in new_catchup_data.iter().enumerate() {
                    if idx < app_state.validator_statuses.len() {
                        let validator_status = &app_state.validator_statuses[idx];
                        
                        // Check node 0
                        if let Some(node_0) = validator_status.nodes_with_status.get(0) {
                            if node_0.status == crate::types::NodeStatus::Standby {
                                let is_caught_up = new_pair.node_0.as_ref()
                                    .map(|c| c.status.contains("Caught up"))
                                    .unwrap_or(false);
                                
                                let has_response = new_pair.node_0.as_ref()
                                    .map(|c| !c.status.contains("Checking"))
                                    .unwrap_or(false);
                                
                                if is_caught_up {
                                    failure_updates.push((idx, 0, 0)); // Reset counter
                                } else if has_response {
                                    // Read current failure count
                                    let current_failures = {
                                        let state = ui_state.read().await;
                                        state.catchup_failure_counts[idx].0
                                    };
                                    
                                    let new_failures = current_failures + 1;
                                    failure_updates.push((idx, 0, new_failures));
                                    
                                    if new_failures >= 3 {
                                        // Check cooldown
                                        let should_alert = {
                                            let state = ui_state.read().await;
                                            match state.last_catchup_alert_times[idx].0 {
                                                Some(last_alert) => last_alert.elapsed().as_secs() >= 300,
                                                None => true,
                                            }
                                        };
                                        
                                        if should_alert {
                                            alerts_to_send.push((
                                                validator_status.validator_pair.identity_pubkey.clone(),
                                                node_0.node.label.clone(),
                                                new_failures,
                                                idx,
                                                0,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        
                        // Check node 1
                        if let Some(node_1) = validator_status.nodes_with_status.get(1) {
                            if node_1.status == crate::types::NodeStatus::Standby {
                                let is_caught_up = new_pair.node_1.as_ref()
                                    .map(|c| c.status.contains("Caught up"))
                                    .unwrap_or(false);
                                
                                let has_response = new_pair.node_1.as_ref()
                                    .map(|c| !c.status.contains("Checking"))
                                    .unwrap_or(false);
                                
                                if is_caught_up {
                                    failure_updates.push((idx, 1, 0)); // Reset counter
                                } else if has_response {
                                    // Read current failure count
                                    let current_failures = {
                                        let state = ui_state.read().await;
                                        state.catchup_failure_counts[idx].1
                                    };
                                    
                                    let new_failures = current_failures + 1;
                                    failure_updates.push((idx, 1, new_failures));
                                    
                                    if new_failures >= 3 {
                                        // Check cooldown
                                        let should_alert = {
                                            let state = ui_state.read().await;
                                            match state.last_catchup_alert_times[idx].1 {
                                                Some(last_alert) => last_alert.elapsed().as_secs() >= 300,
                                                None => true,
                                            }
                                        };
                                        
                                        if should_alert {
                                            alerts_to_send.push((
                                                validator_status.validator_pair.identity_pubkey.clone(),
                                                node_1.node.label.clone(),
                                                new_failures,
                                                idx,
                                                1,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                
                // Send alerts
                for (validator_identity, node_label, consecutive_failures, idx, node_idx) in alerts_to_send {
                    if let Some(alert_mgr) = alert_manager.as_ref() {
                        if let Err(e) = alert_mgr.send_catchup_failure_alert(
                            &validator_identity,
                            &node_label,
                            consecutive_failures,
                        ).await {
                            let _ = log_sender.send(LogMessage {
                                host: node_label.clone(),
                                message: format!("Failed to send catchup alert: {}", e),
                                timestamp: Instant::now(),
                                level: LogLevel::Error,
                            });
                        } else {
                            // Update last alert time
                            let mut state = ui_state.write().await;
                            if node_idx == 0 {
                                state.last_catchup_alert_times[idx].0 = Some(Instant::now());
                            } else {
                                state.last_catchup_alert_times[idx].1 = Some(Instant::now());
                            }
                        }
                    }
                }
                
                // Update UI state
                let mut state = ui_state.write().await;
                
                // Apply failure updates
                for (idx, node_idx, failures) in failure_updates {
                    if node_idx == 0 {
                        state.catchup_failure_counts[idx].0 = failures;
                    } else {
                        state.catchup_failure_counts[idx].1 = failures;
                    }
                }
                
                state.catchup_data = new_catchup_data;
                state.last_catchup_refresh = Instant::now();
            }
        });
        */

        // SSH health monitoring task
        let ui_state = Arc::clone(&self.ui_state);
        let app_state = Arc::clone(&self.app_state);
        let ssh_pool = Arc::clone(&self.ssh_pool);
        let log_sender = self.log_sender.clone();

        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(30));

            // Initialize alert manager and tracker if alerts are configured
            let alert_manager = app_state
                .config
                .alert_config
                .as_ref()
                .filter(|config| config.enabled)
                .map(|config| AlertManager::new(config.clone()));

            let nodes_per_validator = 2;
            let mut alert_tracker = ComprehensiveAlertTracker::new(
                app_state.validator_statuses.len(),
                nodes_per_validator
            );

            loop {
                interval.tick().await;

                // Check SSH health for all nodes
                let mut new_ssh_health_data = Vec::new();

                for (idx, validator_status) in app_state.validator_statuses.iter().enumerate() {
                    let mut node_pair = NodePairSshStatus {
                        node_0: SshHealthStatus {
                            is_healthy: false,
                            last_success: None,
                            failure_start: None,
                        },
                        node_1: SshHealthStatus {
                            is_healthy: false,
                            last_success: None,
                            failure_start: None,
                        },
                    };

                    // Get current state to preserve timing info
                    let current_state = {
                        let state = ui_state.read().await;
                        state.ssh_health_data.get(idx).cloned()
                    };

                    // Check node 0
                    if validator_status.nodes_with_status.len() > 0 {
                        let node_0 = &validator_status.nodes_with_status[0];
                        if let Some(ssh_key) = app_state.detected_ssh_keys.get(&node_0.node.host) {
                            match ssh_pool
                                .execute_command(&node_0.node, ssh_key, "true")
                                .await
                            {
                                Ok(_) => {
                                    node_pair.node_0.is_healthy = true;
                                    node_pair.node_0.last_success = Some(Instant::now());
                                    node_pair.node_0.failure_start = None;
                                    
                                    // Update health tracking
                                    {
                                        let mut state = ui_state.write().await;
                                        state.validator_health[idx].ssh_status.record_success();
                                    }
                                    
                                    let _ = log_sender.send(LogMessage {
                                        host: node_0.node.label.clone(),
                                        message: "SSH health check: OK".to_string(),
                                        timestamp: Instant::now(),
                                        level: LogLevel::Info,
                                    });
                                }
                                Err(e) => {
                                    node_pair.node_0.is_healthy = false;
                                    // Preserve last_success from previous state
                                    if let Some(ref current) = current_state {
                                        node_pair.node_0.last_success = current.node_0.last_success;
                                        // Set failure_start if this is first failure
                                        if current.node_0.is_healthy {
                                            node_pair.node_0.failure_start = Some(Instant::now());
                                        } else {
                                            node_pair.node_0.failure_start = current.node_0.failure_start;
                                        }
                                    } else {
                                        node_pair.node_0.failure_start = Some(Instant::now());
                                    }
                                    
                                    // Update health tracking and check if alert needed
                                    let (should_alert_ssh, consecutive_failures, seconds_since_first) = {
                                        let mut state = ui_state.write().await;
                                        state.validator_health[idx].ssh_status.record_failure(e.to_string());
                                        
                                        let tracker = &state.validator_health[idx].ssh_status;
                                        let consecutive = tracker.consecutive_failures;
                                        let seconds = tracker.seconds_since_first_failure().unwrap_or(0);
                                        
                                        let config = app_state.config.alert_config.as_ref();
                                        let time_threshold = config.map(|c| c.ssh_failure_threshold_seconds).unwrap_or(1800);
                                        
                                        let should_alert = seconds >= time_threshold
                                            && alert_tracker.ssh_failure_tracker[0].should_send_alert(idx);
                                        
                                        (should_alert, consecutive, seconds)
                                    };
                                    
                                    // Send SSH failure alert if needed
                                    if should_alert_ssh {
                                        if let Some(alert_mgr) = alert_manager.as_ref() {
                                            let _ = alert_mgr.send_ssh_failure_alert(
                                                &validator_status.validator_pair.identity_pubkey,
                                                &node_0.node.label,
                                                consecutive_failures,
                                                seconds_since_first,
                                                &e.to_string()
                                            ).await;
                                        }
                                    }
                                    
                                    let _ = log_sender.send(LogMessage {
                                        host: node_0.node.label.clone(),
                                        message: format!("SSH health check failed: {}", e),
                                        timestamp: Instant::now(),
                                        level: LogLevel::Error,
                                    });
                                }
                            }
                        }
                    }

                    // Check node 1
                    if validator_status.nodes_with_status.len() > 1 {
                        let node_1 = &validator_status.nodes_with_status[1];
                        if let Some(ssh_key) = app_state.detected_ssh_keys.get(&node_1.node.host) {
                            match ssh_pool
                                .execute_command(&node_1.node, ssh_key, "true")
                                .await
                            {
                                Ok(_) => {
                                    node_pair.node_1.is_healthy = true;
                                    node_pair.node_1.last_success = Some(Instant::now());
                                    node_pair.node_1.failure_start = None;
                                    
                                    let _ = log_sender.send(LogMessage {
                                        host: node_1.node.label.clone(),
                                        message: "SSH health check: OK".to_string(),
                                        timestamp: Instant::now(),
                                        level: LogLevel::Info,
                                    });
                                }
                                Err(e) => {
                                    node_pair.node_1.is_healthy = false;
                                    // Preserve last_success from previous state
                                    if let Some(ref current) = current_state {
                                        node_pair.node_1.last_success = current.node_1.last_success;
                                        // Set failure_start if this is first failure
                                        if current.node_1.is_healthy {
                                            node_pair.node_1.failure_start = Some(Instant::now());
                                        } else {
                                            node_pair.node_1.failure_start = current.node_1.failure_start;
                                        }
                                    } else {
                                        node_pair.node_1.failure_start = Some(Instant::now());
                                    }
                                    
                                    let _ = log_sender.send(LogMessage {
                                        host: node_1.node.label.clone(),
                                        message: format!("SSH health check failed: {}", e),
                                        timestamp: Instant::now(),
                                        level: LogLevel::Error,
                                    });
                                }
                            }
                        }
                    }

                    new_ssh_health_data.push(node_pair);
                }

                // Update UI state
                let mut state = ui_state.write().await;
                state.ssh_health_data = new_ssh_health_data;
                state.last_ssh_health_refresh = Instant::now();
            }
        });

        // Telegram bot polling has been removed - bot only responds to messages now
    }
}

#[allow(dead_code)]
async fn fetch_catchup_for_node(
    ssh_pool: &AsyncSshPool,
    node: &crate::types::NodeWithStatus,
    ssh_key: &str,
    log_sender: &tokio::sync::mpsc::UnboundedSender<LogMessage>,
) -> Option<CatchupStatus> {
    // Log the executable paths for debugging
    let _ = log_sender.send(LogMessage {
        host: node.node.host.clone(),
        message: format!(
            "Executables - Solana CLI: {:?}, Agave: {:?}, Fdctl: {:?}",
            node.solana_cli_executable, node.agave_validator_executable, node.fdctl_executable
        ),
        timestamp: Instant::now(),
        level: LogLevel::Info,
    });

    let solana_cli = if let Some(cli) = node.solana_cli_executable.as_ref() {
        cli.clone()
    } else if let Some(validator) = node.agave_validator_executable.as_ref() {
        // Try to derive solana CLI path from agave-validator path
        let derived = validator.replace("agave-validator", "solana");
        let _ = log_sender.send(LogMessage {
            host: node.node.host.clone(),
            message: format!(
                "Deriving solana CLI from agave-validator: {} -> {}",
                validator, derived
            ),
            timestamp: Instant::now(),
            level: LogLevel::Info,
        });
        derived
    } else if node.validator_type == crate::types::ValidatorType::Firedancer {
        // For Firedancer, try to use fdctl to get status instead
        if let Some(fdctl) = node.fdctl_executable.as_ref() {
            // Use fdctl status instead of solana catchup for Firedancer
            let status_cmd = format!("{} status", fdctl);
            match ssh_pool
                .execute_command(&node.node, ssh_key, &status_cmd)
                .await
            {
                Ok(output) => {
                    let status = if output.contains("running") {
                        "Caught up".to_string()
                    } else {
                        "Unknown".to_string()
                    };
                    return Some(CatchupStatus {
                        status,
                        last_updated: Instant::now(),
                        is_streaming: false,
                    });
                }
                Err(_) => return None,
            }
        }
        return None;
    } else {
        // Log that we couldn't find solana CLI
        let _ = log_sender.send(LogMessage {
            host: node.node.host.clone(),
            message: "Cannot find solana CLI executable".to_string(),
            timestamp: Instant::now(),
            level: LogLevel::Error,
        });
        return None;
    };

    // First check if the solana CLI exists
    let test_args = vec!["-f", &solana_cli];
    let file_exists = match ssh_pool
        .execute_command_with_args(&node.node, ssh_key, "test", &test_args)
        .await
    {
        Ok(_) => true,
        Err(_) => false,
    };

    if !file_exists {
        let _ = log_sender.send(LogMessage {
            host: node.node.host.clone(),
            message: format!("Solana CLI not found at: {}", solana_cli),
            timestamp: Instant::now(),
            level: LogLevel::Error,
        });
        return Some(CatchupStatus {
            status: "CLI not found".to_string(),
            last_updated: Instant::now(),
            is_streaming: false,
        });
    }

    // Test if we can run solana --version
    let version_args = vec!["--version"];
    match ssh_pool
        .execute_command_with_args(&node.node, ssh_key, &solana_cli, &version_args)
        .await
    {
        Ok(output) => {
            let _ = log_sender.send(LogMessage {
                host: node.node.host.clone(),
                message: format!("Solana CLI version output: {}", output.trim()),
                timestamp: Instant::now(),
                level: LogLevel::Info,
            });
        }
        Err(e) => {
            let _ = log_sender.send(LogMessage {
                host: node.node.host.clone(),
                message: format!("Failed to run solana --version: {}", e),
                timestamp: Instant::now(),
                level: LogLevel::Error,
            });
        }
    }

    // Use args approach for catchup command
    let args = vec!["catchup", "--our-localhost"];

    let _ = log_sender.send(LogMessage {
        host: node.node.host.clone(),
        message: format!(
            "Executing catchup command: {} {}",
            solana_cli,
            args.join(" ")
        ),
        timestamp: Instant::now(),
        level: LogLevel::Info,
    });

    // Try executing the command with args
    match ssh_pool
        .execute_command_with_args(&node.node, ssh_key, &solana_cli, &args)
        .await
    {
        Ok(output) => {
            // Log the raw output for debugging
            let _ = log_sender.send(LogMessage {
                host: node.node.host.clone(),
                message: format!(
                    "Catchup raw output: {}",
                    output.chars().take(200).collect::<String>()
                ),
                timestamp: Instant::now(),
                level: LogLevel::Info,
            });

            let status = if output.contains("0 slot(s)") || output.contains("has caught up") {
                "Caught up".to_string()
            } else if let Some(pos) = output.find(" slot(s) behind") {
                let start = output[..pos].rfind(' ').map(|i| i + 1).unwrap_or(0);
                let slots_str = &output[start..pos];
                if let Ok(slots) = slots_str.parse::<u64>() {
                    format!("{} slots behind", slots)
                } else {
                    "Checking...".to_string()
                }
            } else if output.contains("Error") || output.contains("error") {
                // If there's an error, show a cleaner message
                "Error".to_string()
            } else if output.trim().is_empty() {
                // Try a simple test command to verify SSH is working
                let echo_args = vec!["test"];
                if let Ok(test_output) = ssh_pool
                    .execute_command_with_args(&node.node, ssh_key, "echo", &echo_args)
                    .await
                {
                    if test_output.contains("test") {
                        "No catchup output".to_string()
                    } else {
                        "SSH issue".to_string()
                    }
                } else {
                    "SSH error".to_string()
                }
            } else {
                // For debugging: show first 50 chars of output
                let debug_msg = output.trim().chars().take(50).collect::<String>();
                format!("Unknown: {}", debug_msg)
            };

            let _ = log_sender.send(LogMessage {
                host: node.node.host.clone(),
                message: format!("Catchup status: {}", status),
                timestamp: Instant::now(),
                level: LogLevel::Info,
            });

            Some(CatchupStatus {
                status,
                last_updated: Instant::now(),
                is_streaming: false,
            })
        }
        Err(e) => {
            let _ = log_sender.send(LogMessage {
                host: node.node.host.clone(),
                message: format!("Failed to get catchup status: {}", e),
                timestamp: Instant::now(),
                level: LogLevel::Error,
            });

            None
        }
    }
}

/// Stream catchup status continuously for a single node
async fn stream_catchup_for_node(
    ssh_pool: Arc<AsyncSshPool>,
    node: crate::types::NodeWithStatus,
    ssh_key: String,
    ui_state: Arc<RwLock<UiState>>,
    validator_idx: usize,
    node_idx: usize,
    log_sender: tokio::sync::mpsc::UnboundedSender<LogMessage>,
) {
    loop {
        // Determine the catchup command based on node type
        let catchup_command = if node.validator_type == crate::types::ValidatorType::Firedancer {
            // For Firedancer, use fdctl status
            if let Some(fdctl) = &node.fdctl_executable {
                // Also wrap fdctl in bash -c for consistency
                format!("bash -c '{} status'", fdctl)
            } else {
                // Sleep and retry
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        } else {
            // For Agave/Jito, use solana catchup
            let solana_cli = if let Some(cli) = &node.solana_cli_executable {
                cli.clone()
            } else if let Some(validator) = &node.agave_validator_executable {
                validator.replace("agave-validator", "solana")
            } else {
                // Sleep and retry
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            };
            
            // Need to use bash -c to properly handle the command with its full path
            format!("bash -c '{} catchup --our-localhost 2>&1'", solana_cli)
        };
        
        // Log the command being executed
        let _ = log_sender.send(LogMessage {
            host: node.node.host.clone(),
            message: format!("Starting catchup stream with command: {}", catchup_command),
            timestamp: Instant::now(),
            level: LogLevel::Info,
        });
        
        // Create channel for streaming output
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(100);
        
        // Start the streaming command
        let stream_task = ssh_pool.execute_command_streaming(
            &node.node,
            &ssh_key,
            &catchup_command,
            tx,
        );
        
        // Process streaming output
        let ui_state_clone = Arc::clone(&ui_state);
        let is_firedancer = node.validator_type == crate::types::ValidatorType::Firedancer;
        let process_task = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                let last_output = line.trim().to_string();
                
                // Update UI state with the latest output
                let mut state = ui_state_clone.write().await;
                if let Some(catchup_data) = state.catchup_data.get_mut(validator_idx) {
                    let status = parse_catchup_output(&last_output, is_firedancer);
                    
                    let catchup_status = CatchupStatus {
                        status,
                        last_updated: Instant::now(),
                        is_streaming: true,
                    };
                    
                    if node_idx == 0 {
                        catchup_data.node_0 = Some(catchup_status);
                    } else {
                        catchup_data.node_1 = Some(catchup_status);
                    }
                }
            }
        });
        
        // Wait for either task to complete
        tokio::select! {
            result = stream_task => {
                if let Err(e) = result {
                    let _ = log_sender.send(LogMessage {
                        host: node.node.host.clone(),
                        message: format!("Catchup streaming error: {}", e),
                        timestamp: Instant::now(),
                        level: LogLevel::Error,
                    });
                }
            }
            _ = process_task => {
                // Processing task completed
            }
        }
        
        // Mark as not streaming anymore
        {
            let mut state = ui_state.write().await;
            if let Some(catchup_data) = state.catchup_data.get_mut(validator_idx) {
                if node_idx == 0 {
                    if let Some(ref mut status) = catchup_data.node_0 {
                        status.is_streaming = false;
                    }
                } else {
                    if let Some(ref mut status) = catchup_data.node_1 {
                        status.is_streaming = false;
                    }
                }
            }
        }
        
        // Wait before retrying
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Parse catchup output to extract status
fn parse_catchup_output(output: &str, is_firedancer: bool) -> String {
    if is_firedancer {
        // For Firedancer, check if it's running
        if output.contains("running") {
            "Caught up".to_string()
        } else {
            "Not running".to_string()
        }
    } else {
        // For Agave/Jito, parse the catchup output
        if output.contains("0 slot(s)") || output.contains("has caught up") {
            "Caught up".to_string()
        } else if let Some(pos) = output.find(" slot(s) behind") {
            let start = output[..pos].rfind(' ').map(|i| i + 1).unwrap_or(0);
            let slots_str = &output[start..pos];
            if let Ok(slots) = slots_str.parse::<u64>() {
                format!("{} slots behind", slots)
            } else {
                output.to_string()
            }
        } else if output.contains("bash:") && output.contains("line") {
            // Parse bash errors more nicely
            if output.contains("command not found") || output.contains("No such file") {
                "CLI not found".to_string()
            } else {
                "Command error".to_string()
            }
        } else if output.contains("Error") || output.contains("error") {
            if output.contains("RPC") {
                "RPC Error".to_string()
            } else if output.contains("connection") {
                "Connection Error".to_string()
            } else {
                "Error".to_string()
            }
        } else if output.trim().is_empty() {
            "Waiting...".to_string()
        } else {
            // Show the raw output if we can't parse it, but limit length
            let trimmed = output.trim();
            if trimmed.len() > 40 {
                format!("{}...", trimmed.chars().take(37).collect::<String>())
            } else {
                trimmed.to_string()
            }
        }
    }
}

/// Run the enhanced UI
/// Returns true if a switch was confirmed, false otherwise
pub async fn run_enhanced_ui(app: &mut EnhancedStatusApp) -> Result<bool> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    terminal.hide_cursor()?;

    // Spawn background tasks
    app.spawn_background_tasks();

    // Process log messages in background (keeping for internal use but not displaying)
    // Note: log messages are now consumed by the Telegram bot if enabled
    
    // Trigger an initial refresh when starting the UI
    {
        // Set refresh flags immediately so UI shows refreshing state
        let mut ui_state_write = app.ui_state.write().await;
        for refresh_state in ui_state_write.field_refresh_states.iter_mut() {
            refresh_state.node_0.status_refreshing = true;
            refresh_state.node_0.identity_refreshing = true;
            refresh_state.node_0.version_refreshing = true;
            refresh_state.node_1.status_refreshing = true;
            refresh_state.node_1.identity_refreshing = true;
            refresh_state.node_1.version_refreshing = true;
        }
        drop(ui_state_write);
        
        let app_state_clone = app.app_state.clone();
        let ui_state_clone = app.ui_state.clone();
        tokio::spawn(async move {
            refresh_all_fields(app_state_clone, ui_state_clone).await;
        });
    }

    // Main UI loop
    let mut ui_interval = interval(Duration::from_millis(100)); // 10 FPS

    let mut emergency_mode = false;
    
    loop {
        // Check for quit signal
        if *app.should_quit.read().await {
            break;
        }

        // Check if emergency takeover is in progress
        let emergency_in_progress = *app.emergency_takeover_in_progress.read().await;
        
        if emergency_in_progress && !emergency_mode {
            // Just entering emergency mode - cleanup terminal
            emergency_mode = true;
            terminal.clear()?;
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
            terminal.show_cursor()?;
        } else if !emergency_in_progress && emergency_mode {
            // Just exiting emergency mode - restore terminal
            emergency_mode = false;
            enable_raw_mode()?;
            execute!(terminal.backend_mut(), EnterAlternateScreen)?;
            terminal.clear()?;
            terminal.hide_cursor()?;
        }
        
        if emergency_in_progress {
            // During emergency takeover, just wait without rendering
            ui_interval.tick().await;
            continue;
        }

        // Handle keyboard events
        if event::poll(Duration::from_millis(10))? {
            if let Event::Key(key) = event::read()? {
                // Only handle key press events, not key releases
                if key.kind == crossterm::event::KeyEventKind::Press {
                    handle_key_event(
                        key,
                        &app.ui_state,
                        &app.should_quit,
                        &app.view_state,
                        &app.app_state,
                        &app.switch_confirmed,
                    )
                    .await?;
                }
            }
        }

        // Draw UI based on current view
        let ui_state_read = app.ui_state.read().await;
        let view_state_read = app.view_state.read().await;

        terminal.draw(|f| match *view_state_read {
            ViewState::Status => draw_ui(f, &ui_state_read, &app.app_state),
            ViewState::Switch => draw_switch_ui(f, &app.app_state),
        })?;

        drop(ui_state_read);
        drop(view_state_read);

        // Wait for next frame
        ui_interval.tick().await;
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // Return whether switch was confirmed
    Ok(*app.switch_confirmed.read().await)
}

/// Handle keyboard events
async fn handle_key_event(
    key: KeyEvent,
    ui_state: &Arc<RwLock<UiState>>,
    should_quit: &Arc<RwLock<bool>>,
    view_state: &Arc<RwLock<ViewState>>,
    _app_state: &Arc<AppState>,
    switch_confirmed: &Arc<RwLock<bool>>,
) -> Result<()> {
    // Don't hold a write lock for the entire function!
    
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            let current_view = *view_state.read().await;
            if current_view == ViewState::Switch {
                // In switch view, go back to status view
                let mut view = view_state.write().await;
                *view = ViewState::Status;
                
                // Trigger a refresh when returning to status view
                let app_state_clone = _app_state.clone();
                let ui_state_clone = ui_state.clone();
                tokio::spawn(async move {
                    refresh_all_fields(app_state_clone, ui_state_clone).await;
                });
            } else {
                // In status view, quit the application
                *should_quit.write().await = true;
            }
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            *should_quit.write().await = true;
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            // Show switch confirmation view
            let mut view = view_state.write().await;
            *view = ViewState::Switch;
        }
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            // Confirm and execute switch if in switch view
            let current_view = *view_state.read().await;
            if current_view == ViewState::Switch {
                // Set switch confirmed flag and quit to perform switch
                *switch_confirmed.write().await = true;
                *should_quit.write().await = true;
                // Force immediate exit from the event loop
                return Ok(());
            }
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            // Refresh fields in the validator status view
            let is_status_view = matches!(*view_state.read().await, ViewState::Status);
            
            if is_status_view {
                // Set refresh states immediately before spawning
                {
                    let mut ui_state_write = ui_state.write().await;
                    ui_state_write.is_refreshing = true;
                    
                    // Set all field refresh states to true immediately
                    for refresh_state in ui_state_write.field_refresh_states.iter_mut() {
                        refresh_state.node_0.status_refreshing = true;
                        refresh_state.node_0.identity_refreshing = true;
                        refresh_state.node_0.version_refreshing = true;
                        refresh_state.node_1.status_refreshing = true;
                        refresh_state.node_1.identity_refreshing = true;
                        refresh_state.node_1.version_refreshing = true;
                    }
                }
                
                // Clone what we need after setting flags
                let app_state_clone = _app_state.clone();
                let ui_state_clone = ui_state.clone();
                
                // Spawn the refresh operation
                tokio::spawn(async move {
                    refresh_all_fields(app_state_clone, ui_state_clone).await;
                });
            }
        }
        _ => {}
    }

    Ok(())
}

/// Draw the main UI
fn draw_ui(f: &mut ratatui::Frame, ui_state: &UiState, app_state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),    // Validator tables take all remaining space
            Constraint::Length(1), // Footer
        ])
        .split(f.size());

    // Draw validator summaries
    draw_validator_summaries(f, chunks[0], ui_state, app_state);

    // Draw footer
    draw_footer(f, chunks[1], ui_state);
}

#[allow(dead_code)]
fn draw_header(f: &mut ratatui::Frame, area: Rect, _ui_state: &UiState) {
    // Just leave empty - header will be in the table border
    let header = Paragraph::new("");
    f.render_widget(header, area);
}

fn draw_validator_summaries(
    f: &mut ratatui::Frame,
    area: Rect,
    ui_state: &UiState,
    _app_state: &AppState,
) {
    // Use validator statuses from UI state
    let validator_statuses = &ui_state.validator_statuses;
    let validator_count = validator_statuses.len();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![
            Constraint::Percentage(100 / validator_count as u16);
            validator_count
        ])
        .split(area);

    for (idx, (validator_status, chunk)) in validator_statuses
        .iter()
        .zip(chunks.iter())
        .enumerate()
    {
        let vote_data = ui_state.vote_data.get(idx).and_then(|v| v.as_ref());
        let catchup_data = ui_state.catchup_data.get(idx);
        let prev_slot = ui_state.previous_last_slots.get(idx).and_then(|&v| v);
        let inc_time = ui_state.increment_times.get(idx).and_then(|&v| v);
        let ssh_health_data = ui_state.ssh_health_data.get(idx);

        let field_refresh_state = ui_state.field_refresh_states.get(idx);
        draw_side_by_side_tables(
            f,
            *chunk,
            validator_status,
            vote_data,
            catchup_data,
            prev_slot,
            inc_time,
            _app_state,
            ui_state.last_catchup_refresh,
            ssh_health_data,
            ui_state.last_ssh_health_refresh,
            field_refresh_state,
        );
    }
}

fn draw_side_by_side_tables(
    f: &mut ratatui::Frame,
    area: Rect,
    validator_status: &crate::ValidatorStatus,
    vote_data: Option<&ValidatorVoteData>,
    catchup_data: Option<&NodePairStatus>,
    previous_last_slot: Option<u64>,
    increment_time: Option<Instant>,
    app_state: &AppState,
    last_catchup_refresh: Instant,
    ssh_health_data: Option<&NodePairSshStatus>,
    last_ssh_health_refresh: Instant,
    field_refresh_state: Option<&NodeFieldRefreshState>,
) {
    // Split area horizontally
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(area);

    // Always show nodes in the same order (node 0 on left, node 1 on right)
    // This keeps the hosts in consistent positions
    let (left_node_idx, right_node_idx) = (0, 1);

    // Draw left table (node 0)
    if let Some(node) = validator_status.nodes_with_status.get(left_node_idx) {
        let catchup_status = catchup_data.and_then(|c| {
            if left_node_idx == 0 { c.node_0.as_ref() } else { c.node_1.as_ref() }
        });
        let ssh_health = ssh_health_data.and_then(|s| {
            if left_node_idx == 0 { Some(&s.node_0) } else { Some(&s.node_1) }
        });
        
        let node_refresh_state = field_refresh_state.map(|s| {
            if left_node_idx == 0 { &s.node_0 } else { &s.node_1 }
        });
        
        draw_single_node_table(
            f,
            chunks[0],
            validator_status,
            node,
            vote_data,
            catchup_status,
            previous_last_slot,
            increment_time,
            app_state,
            last_catchup_refresh,
            ssh_health,
            last_ssh_health_refresh,
            node_refresh_state,
            true, // is_left_table
        );
    }

    // Draw right table (node 1)
    if let Some(node) = validator_status.nodes_with_status.get(right_node_idx) {
        let catchup_status = catchup_data.and_then(|c| {
            if right_node_idx == 0 { c.node_0.as_ref() } else { c.node_1.as_ref() }
        });
        let ssh_health = ssh_health_data.and_then(|s| {
            if right_node_idx == 0 { Some(&s.node_0) } else { Some(&s.node_1) }
        });
        
        let node_refresh_state = field_refresh_state.map(|s| {
            if right_node_idx == 0 { &s.node_0 } else { &s.node_1 }
        });
        
        draw_single_node_table(
            f,
            chunks[1],
            validator_status,
            node,
            vote_data,
            catchup_status,
            previous_last_slot,
            increment_time,
            app_state,
            last_catchup_refresh,
            ssh_health,
            last_ssh_health_refresh,
            node_refresh_state,
            false, // is_left_table
        );
    }
}

fn draw_single_node_table(
    f: &mut ratatui::Frame,
    area: Rect,
    validator_status: &crate::ValidatorStatus,
    node: &crate::types::NodeWithStatus,
    vote_data: Option<&ValidatorVoteData>,
    catchup_status: Option<&CatchupStatus>,
    previous_last_slot: Option<u64>,
    increment_time: Option<Instant>,
    app_state: &AppState,
    _last_catchup_refresh: Instant,
    ssh_health: Option<&SshHealthStatus>,
    last_ssh_health_refresh: Instant,
    field_refresh_state: Option<&FieldRefreshStates>,
    _is_left_table: bool,
) {
    // Add padding around the table
    let padded_area = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    
    let mut rows = vec![];

    // Node Status (first row)
    let status_display = if field_refresh_state.map_or(false, |s| s.status_refreshing) {
        format!("🔄 Checking... ({})", node.node.label)
    } else {
        format!(
            "{} ({})",
            match node.status {
                crate::types::NodeStatus::Active => "🟢 ACTIVE",
                crate::types::NodeStatus::Standby => "🟡 STANDBY",
                crate::types::NodeStatus::Unknown => "🔴 UNKNOWN",
            },
            node.node.label
        )
    };
    
    rows.push(Row::new(vec![
        Cell::from("Status"),
        Cell::from(status_display.clone())
        .style(Style::default().fg(
            if field_refresh_state.map_or(false, |s| s.status_refreshing) {
                Color::DarkGray
            } else {
                match node.status {
                    crate::types::NodeStatus::Active => Color::Green,
                    crate::types::NodeStatus::Standby => Color::Yellow,
                    crate::types::NodeStatus::Unknown => Color::Red,
                }
            }
        )),
    ]));

    // Vote account info
    let vote_key = &validator_status.validator_pair.vote_pubkey;
    rows.push(Row::new(vec![
        Cell::from("Vote"),
        Cell::from(vote_key.clone()),
    ]));

    // Identity
    let identity_display = if field_refresh_state.map_or(false, |s| s.identity_refreshing) {
        "🔄 Refreshing...".to_string()
    } else {
        node.current_identity.as_deref().unwrap_or("Unknown").to_string()
    };
    rows.push(Row::new(vec![
        Cell::from("Identity"),
        Cell::from(identity_display),
    ]));

    // Host info
    rows.push(Row::new(vec![
        Cell::from("Host"),
        Cell::from(node.node.host.as_str()),
    ]));

    // Validator type and version
    let client_display = if field_refresh_state.map_or(false, |s| s.version_refreshing) {
        "🔄 Detecting...".to_string()
    } else {
        let version = node.version.as_deref().unwrap_or("");
        let cleaned_version = version
            .replace("Firedancer ", "")
            .replace("Agave ", "")
            .replace("Jito ", "");
        format!(
            "{} {}",
            match node.validator_type {
                crate::types::ValidatorType::Firedancer => "Firedancer",
                crate::types::ValidatorType::Agave => "Agave",
                crate::types::ValidatorType::Jito => "Jito",
                crate::types::ValidatorType::Unknown => "Unknown",
            },
            cleaned_version
        )
    };
    
    rows.push(Row::new(vec![
        Cell::from("Client"),
        Cell::from(client_display),
    ]));

    // Swap readiness
    rows.push(Row::new(vec![
        Cell::from("Swap Ready"),
        Cell::from(if node.swap_ready.unwrap_or(false) {
            "✅ Ready"
        } else {
            "❌ Not Ready"
        })
        .style(Style::default().fg(if node.swap_ready.unwrap_or(false) {
            Color::Green
        } else {
            Color::Red
        })),
    ]));

    // Sync status if available
    if let Some(sync_status) = &node.sync_status {
        rows.push(Row::new(vec![
            Cell::from("Sync Status"),
            Cell::from(sync_status.as_str()),
        ]));
    }

    // Section separator before Executable Paths
    rows.push(create_section_header_with_label("PATHS"));

    // Ledger path
    if let Some(ledger_path) = &node.ledger_path {
        rows.push(Row::new(vec![
            Cell::from("Ledger Path"),
            Cell::from(
                ledger_path
                    .split('/')
                    .last()
                    .unwrap_or("N/A"),
            ),
        ]));
    }

    // Executable paths
    if let Some(solana_cli) = &node.solana_cli_executable {
        rows.push(Row::new(vec![
            Cell::from("Solana CLI"),
            Cell::from(shorten_path(solana_cli, 30)),
        ]));
    }

    if let Some(fdctl) = &node.fdctl_executable {
        rows.push(Row::new(vec![
            Cell::from("Fdctl Path"),
            Cell::from(shorten_path(fdctl, 30)),
        ]));
    }

    if let Some(agave) = &node.agave_validator_executable {
        rows.push(Row::new(vec![
            Cell::from("Agave Path"),
            Cell::from(shorten_path(agave, 30)),
        ]));
    }

    // Section separator before Vote
    rows.push(create_section_header_with_label("VOTE STATUS"));

    // Catchup/Status display
    let row_label = if node.validator_type == crate::types::ValidatorType::Firedancer {
        "Status"  // For Firedancer, show as "Status" since fdctl status shows running state
    } else {
        "Catchup" // For Agave/Jito, show as "Catchup"
    };
    
    // Show catchup/status for standby nodes and Firedancer nodes (regardless of active/standby)
    if node.status == crate::types::NodeStatus::Standby || node.validator_type == crate::types::ValidatorType::Firedancer {
        if let Some(catchup) = catchup_status {
            let status_display = if catchup.is_streaming {
                // Add special handling for errors during streaming
                if catchup.status.starts_with("[ERROR]") {
                    // Show a cleaner error message
                    "❌ Command failed".to_string()
                } else {
                    format!("🔄 {}", catchup.status)
                }
            } else if catchup.status == "Waiting..." {
                "⏳ Starting...".to_string()
            } else if catchup.status == "CLI not found" {
                "❌ Solana CLI not found".to_string()
            } else if catchup.status == "Command error" {
                "❌ Command error".to_string()
            } else {
                catchup.status.clone()
            };

            rows.push(Row::new(vec![
                Cell::from(row_label),
                Cell::from(status_display.clone()).style(if status_display.contains("Caught up") {
                    Style::default().fg(Color::Green)
                } else if status_display.contains("Error") || status_display.contains("not found") {
                    Style::default().fg(Color::Red)
                } else if status_display.contains("🔄") || status_display.contains("⏳") {
                    Style::default().fg(Color::DarkGray)
                } else if status_display.contains("behind") {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::White)
                }),
            ]));
        } else {
            // No catchup data yet
            rows.push(Row::new(vec![
                Cell::from(row_label),
                Cell::from("⏳ Initializing...").style(Style::default().fg(Color::DarkGray)),
            ]));
        }
    } else {
        // Active Agave/Jito nodes don't need catchup
        rows.push(Row::new(vec![
            Cell::from(row_label),
            Cell::from("-").style(Style::default().fg(Color::DarkGray)),
        ]));
    }

    // Vote status - always show
    let is_active = node.status == crate::types::NodeStatus::Active;
    
    let (vote_display, vote_style) = if !is_active {
        // Non-active nodes always show "-"
        ("-".to_string(), Style::default())
    } else if let Some(vote_data) = vote_data {
        // Active node with vote data
        let last_slot_info = vote_data.recent_votes.last().map(|lv| lv.slot);
        
        let mut display = if vote_data.is_voting {
            "✅ Voting".to_string()
        } else {
            "⚠️ Not Voting".to_string()
        };
        
        if let Some(last_slot) = last_slot_info {
            display.push_str(&format!(" - {}", last_slot));
            
            if let Some(prev) = previous_last_slot {
                if last_slot > prev {
                    let inc = format!(" (+{})", last_slot - prev);
                    display.push_str(&inc);
                }
            }
        }
        
        let has_recent_increment = if let Some(prev) = previous_last_slot {
            last_slot_info.map(|slot| slot > prev).unwrap_or(false)
                && increment_time.map(|t| t.elapsed().as_secs() < 3).unwrap_or(false)
        } else {
            false
        };
        
        let style = if has_recent_increment {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else if vote_data.is_voting {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Yellow)
        };
        
        (display, style)
    } else {
        // Active node but no vote data yet
        ("-".to_string(), Style::default())
    };

    rows.push(Row::new(vec![
        Cell::from("Vote Status"),
        Cell::from(vote_display).style(vote_style),
    ]));

    // Section separator before SSH
    rows.push(create_section_header_with_label("HEALTH"));

    // Node health status
    let health_display = if let Some(health) = ssh_health {
        let elapsed = last_ssh_health_refresh.elapsed().as_secs();
        let next_check_in = if elapsed >= 30 { 0 } else { 30 - elapsed };
        
        if health.is_healthy {
            if next_check_in > 0 {
                format!("✅ Healthy (next check in {}s)", next_check_in)
            } else {
                "✅ Healthy (checking...)".to_string()
            }
        } else {
            let failure_duration = health.failure_start
                .map(|start| start.elapsed())
                .unwrap_or_else(|| Duration::from_secs(0));
            
            let duration_str = if failure_duration.as_secs() < 60 {
                format!("{}s", failure_duration.as_secs())
            } else if failure_duration.as_secs() < 3600 {
                format!("{}m", failure_duration.as_secs() / 60)
            } else {
                format!("{}h", failure_duration.as_secs() / 3600)
            };
            
            format!("❌ Failed (for {})", duration_str)
        }
    } else {
        "⏳ Checking...".to_string()
    };
    
    rows.push(Row::new(vec![
        Cell::from("Node Health"),
        Cell::from(health_display.clone()).style(
            if health_display.contains("Healthy") {
                Style::default().fg(Color::Green)
            } else if health_display.contains("Failed") {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Yellow)
            }
        ),
    ]));

    // Section separator before Alert Configuration
    rows.push(create_section_header_with_label("ALERTS"));

    // Alert Configuration
    match &app_state.config.alert_config {
        Some(alert_config) if alert_config.enabled => {
            // Alert Status
            let alert_method = if alert_config.telegram.is_some() {
                "✅ Telegram"
            } else {
                "⚠️ Enabled (no method)"
            };
            rows.push(Row::new(vec![
                Cell::from("Alert Status"),
                Cell::from(alert_method).style(Style::default().fg(
                    if alert_config.telegram.is_some() { Color::Green } else { Color::Yellow }
                )),
            ]));

            // Delinquency threshold
            rows.push(Row::new(vec![
                Cell::from("Delinquency"),
                Cell::from(format!("{}s threshold", alert_config.delinquency_threshold_seconds))
                    .style(Style::default().fg(Color::Red)),
            ]));

            // SSH failure threshold
            rows.push(Row::new(vec![
                Cell::from("SSH Failure"),
                Cell::from(format!("{}m threshold", alert_config.ssh_failure_threshold_seconds / 60))
                    .style(Style::default().fg(Color::Yellow)),
            ]));

            // RPC failure threshold
            rows.push(Row::new(vec![
                Cell::from("RPC Failure"),
                Cell::from(format!("{}m threshold", alert_config.rpc_failure_threshold_seconds / 60))
                    .style(Style::default().fg(Color::Yellow)),
            ]));
            
            // Auto-failover status
            rows.push(Row::new(vec![
                Cell::from("Auto-Failover"),
                Cell::from(if alert_config.auto_failover_enabled { 
                    "✅ Enabled" 
                } else { 
                    "❌ Disabled" 
                })
                .style(Style::default().fg(
                    if alert_config.auto_failover_enabled { Color::Green } else { Color::Red }
                )),
            ]));
        }
        _ => {
            rows.push(Row::new(vec![
                Cell::from("Alert Status"),
                Cell::from("❌ Disabled").style(Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    // Highlight border based on node status, not position
    let border_style = if node.status == crate::types::NodeStatus::Active {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let table = Table::new(
        rows,
        vec![
            Constraint::Length(20),
            Constraint::Percentage(80),
        ],
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .padding(ratatui::widgets::Padding::new(1, 1, 0, 0)),
    );

    f.render_widget(table, padded_area);
}

fn create_section_header_with_label(label: &'static str) -> Row<'static> {
    if label.is_empty() {
        // Empty row for spacing
        Row::new(vec![
            Cell::from(""),
            Cell::from(""),
        ])
        .height(1)
    } else {
        // Section label
        Row::new(vec![
            Cell::from(label),
            Cell::from(""),
        ])
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM))
        .height(1)
    }
}

#[allow(dead_code)]
fn draw_validator_table(
    f: &mut ratatui::Frame,
    area: Rect,
    validator_status: &crate::ValidatorStatus,
    vote_data: Option<&ValidatorVoteData>,
    catchup_data: Option<&NodePairStatus>,
    previous_last_slot: Option<u64>,
    increment_time: Option<Instant>,
    app_state: &AppState,
    last_catchup_refresh: Instant,
) {
    // Add padding around the table
    let padded_area = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    
    let vote_key = &validator_status.validator_pair.vote_pubkey;
    let vote_formatted = format!(
        "{}…{}",
        vote_key.chars().take(4).collect::<String>(),
        vote_key
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    );

    let identity_key = &validator_status.validator_pair.identity_pubkey;
    let identity_formatted = format!(
        "{}…{}",
        identity_key.chars().take(4).collect::<String>(),
        identity_key
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    );

    let _validator_name = validator_status
        .metadata
        .as_ref()
        .and_then(|m| m.name.as_ref())
        .cloned()
        .unwrap_or_else(|| vote_formatted.clone());

    let mut rows = vec![];

    // Node status row with host and status
    if validator_status.nodes_with_status.len() >= 2 {
        let node_0 = &validator_status.nodes_with_status[0];
        let node_1 = &validator_status.nodes_with_status[1];

        // Status row
        rows.push(Row::new(vec![
            Cell::from("Status"),
            Cell::from(format!(
                "{} ({})",
                match node_0.status {
                    crate::types::NodeStatus::Active => "🟢 ACTIVE",
                    crate::types::NodeStatus::Standby => "🟡 STANDBY",
                    crate::types::NodeStatus::Unknown => "🔴 UNKNOWN",
                },
                node_0.node.label
            ))
            .style(Style::default().fg(match node_0.status {
                crate::types::NodeStatus::Active => Color::Green,
                crate::types::NodeStatus::Standby => Color::Yellow,
                crate::types::NodeStatus::Unknown => Color::Red,
            })),
            Cell::from(format!(
                "{} ({})",
                match node_1.status {
                    crate::types::NodeStatus::Active => "🟢 ACTIVE",
                    crate::types::NodeStatus::Standby => "🟡 STANDBY",
                    crate::types::NodeStatus::Unknown => "🔴 UNKNOWN",
                },
                node_1.node.label
            ))
            .style(Style::default().fg(match node_1.status {
                crate::types::NodeStatus::Active => Color::Green,
                crate::types::NodeStatus::Standby => Color::Yellow,
                crate::types::NodeStatus::Unknown => Color::Red,
            })),
        ]));

        // Host info row
        rows.push(Row::new(vec![
            Cell::from("Host"),
            Cell::from(node_0.node.host.as_str()),
            Cell::from(node_1.node.host.as_str()),
        ]));

        // Validator type and version row
        rows.push(Row::new(vec![
            Cell::from("Type/Version"),
            Cell::from({
                let version = node_0.version.as_deref().unwrap_or("");
                let cleaned_version = version
                    .replace("Firedancer ", "")
                    .replace("Agave ", "")
                    .replace("Jito ", "");
                format!(
                    "{} {}",
                    match node_0.validator_type {
                        crate::types::ValidatorType::Firedancer => "Firedancer",
                        crate::types::ValidatorType::Agave => "Agave",
                        crate::types::ValidatorType::Jito => "Jito",
                        crate::types::ValidatorType::Unknown => "Unknown",
                    },
                    cleaned_version
                )
            }),
            Cell::from({
                let version = node_1.version.as_deref().unwrap_or("");
                let cleaned_version = version
                    .replace("Firedancer ", "")
                    .replace("Agave ", "")
                    .replace("Jito ", "");
                format!(
                    "{} {}",
                    match node_1.validator_type {
                        crate::types::ValidatorType::Firedancer => "Firedancer",
                        crate::types::ValidatorType::Agave => "Agave",
                        crate::types::ValidatorType::Jito => "Jito",
                        crate::types::ValidatorType::Unknown => "Unknown",
                    },
                    cleaned_version
                )
            }),
        ]));

        // Identity row - format as ascd...edsas
        let id0 = node_0.current_identity.as_deref().unwrap_or("Unknown");
        let id1 = node_1.current_identity.as_deref().unwrap_or("Unknown");
        let id0_formatted = if id0 != "Unknown" && id0.len() > 8 {
            format!(
                "{}…{}",
                id0.chars().take(4).collect::<String>(),
                id0.chars()
                    .rev()
                    .take(4)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            )
        } else {
            id0.to_string()
        };
        let id1_formatted = if id1 != "Unknown" && id1.len() > 8 {
            format!(
                "{}…{}",
                id1.chars().take(4).collect::<String>(),
                id1.chars()
                    .rev()
                    .take(4)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            )
        } else {
            id1.to_string()
        };

        rows.push(Row::new(vec![
            Cell::from("Identity"),
            Cell::from(id0_formatted),
            Cell::from(id1_formatted),
        ]));

        // Swap readiness row
        rows.push(Row::new(vec![
            Cell::from("Swap Ready"),
            Cell::from(if node_0.swap_ready.unwrap_or(false) {
                "✅ Ready"
            } else {
                "❌ Not Ready"
            })
            .style(Style::default().fg(if node_0.swap_ready.unwrap_or(false) {
                Color::Green
            } else {
                Color::Red
            })),
            Cell::from(if node_1.swap_ready.unwrap_or(false) {
                "✅ Ready"
            } else {
                "❌ Not Ready"
            })
            .style(Style::default().fg(if node_1.swap_ready.unwrap_or(false) {
                Color::Green
            } else {
                Color::Red
            })),
        ]));

        // Sync status row if available
        if node_0.sync_status.is_some() || node_1.sync_status.is_some() {
            rows.push(Row::new(vec![
                Cell::from("Sync Status"),
                Cell::from(node_0.sync_status.as_deref().unwrap_or("N/A")),
                Cell::from(node_1.sync_status.as_deref().unwrap_or("N/A")),
            ]));
        }

        // Ledger path row if available
        if node_0.ledger_path.is_some() || node_1.ledger_path.is_some() {
            rows.push(Row::new(vec![
                Cell::from("Ledger Path"),
                Cell::from(
                    node_0
                        .ledger_path
                        .as_deref()
                        .unwrap_or("N/A")
                        .split('/')
                        .last()
                        .unwrap_or("N/A"),
                ),
                Cell::from(
                    node_1
                        .ledger_path
                        .as_deref()
                        .unwrap_or("N/A")
                        .split('/')
                        .last()
                        .unwrap_or("N/A"),
                ),
            ]));
        }

        // Executable paths - shortened to save space
        if node_0.solana_cli_executable.is_some() || node_1.solana_cli_executable.is_some() {
            rows.push(Row::new(vec![
                Cell::from("Solana CLI"),
                Cell::from(shorten_path(
                    node_0.solana_cli_executable.as_deref().unwrap_or("N/A"),
                    30,
                )),
                Cell::from(shorten_path(
                    node_1.solana_cli_executable.as_deref().unwrap_or("N/A"),
                    30,
                )),
            ]));
        }

        if node_0.fdctl_executable.is_some() || node_1.fdctl_executable.is_some() {
            rows.push(Row::new(vec![
                Cell::from("Fdctl Path"),
                Cell::from(shorten_path(
                    node_0.fdctl_executable.as_deref().unwrap_or("N/A"),
                    30,
                )),
                Cell::from(shorten_path(
                    node_1.fdctl_executable.as_deref().unwrap_or("N/A"),
                    30,
                )),
            ]));
        }

        if node_0.agave_validator_executable.is_some()
            || node_1.agave_validator_executable.is_some()
        {
            rows.push(Row::new(vec![
                Cell::from("Agave Path"),
                Cell::from(shorten_path(
                    node_0
                        .agave_validator_executable
                        .as_deref()
                        .unwrap_or("N/A"),
                    30,
                )),
                Cell::from(shorten_path(
                    node_1
                        .agave_validator_executable
                        .as_deref()
                        .unwrap_or("N/A"),
                    30,
                )),
            ]));
        }

        // Catchup status
        if let Some(catchup) = catchup_data {
            // Calculate seconds until next catchup check first
            let elapsed = last_catchup_refresh.elapsed().as_secs();
            let next_check_in = if elapsed >= 30 { 0 } else { 30 - elapsed };
            let next_check_suffix = if next_check_in > 0 {
                format!(" (next in {}s)", next_check_in)
            } else {
                String::new()
            };

            let node_0_status = catchup
                .node_0
                .as_ref()
                .map(|c| {
                    let status = if c.status == "Checking..." {
                        "🔄 Checking...".to_string()
                    } else {
                        c.status.clone()
                    };
                    // Add countdown suffix for non-checking states
                    if !status.contains("Checking") && next_check_in > 0 {
                        format!("{}{}", status, next_check_suffix)
                    } else {
                        status
                    }
                })
                .unwrap_or_else(|| "🔄 Checking...".to_string());
            let node_1_status = catchup
                .node_1
                .as_ref()
                .map(|c| {
                    let status = if c.status == "Checking..." {
                        "🔄 Checking...".to_string()
                    } else {
                        c.status.clone()
                    };
                    // Add countdown suffix for non-checking states
                    if !status.contains("Checking") && next_check_in > 0 {
                        format!("{}{}", status, next_check_suffix)
                    } else {
                        status
                    }
                })
                .unwrap_or_else(|| "🔄 Checking...".to_string());

            rows.push(Row::new(vec![
                Cell::from("Catchup"),
                Cell::from(node_0_status.clone()).style(if node_0_status.contains("Caught up") {
                    Style::default().fg(Color::Green)
                } else if node_0_status.contains("Error") {
                    Style::default().fg(Color::Red)
                } else if node_0_status.contains("Checking") {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Yellow)
                }),
                Cell::from(node_1_status.clone()).style(if node_1_status.contains("Caught up") {
                    Style::default().fg(Color::Green)
                } else if node_1_status.contains("Error") {
                    Style::default().fg(Color::Red)
                } else if node_1_status.contains("Checking") {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Yellow)
                }),
            ]));
        }

        // Vote status row with slot info - moved to bottom
        if let Some(vote_data) = vote_data {
            let last_slot_info = vote_data.recent_votes.last().map(|lv| lv.slot);
            
            // Build vote status with slot info
            let build_vote_display = |is_active: bool| -> (String, Style) {
                if !is_active {
                    return ("-".to_string(), Style::default());
                }
                
                let mut display = if vote_data.is_voting {
                    "✅ Voting".to_string()
                } else {
                    "⚠️ Not Voting".to_string()
                };
                
                // Add slot info if available
                if let Some(last_slot) = last_slot_info {
                    display.push_str(&format!(" - {}", last_slot));
                    
                    // Add increment if applicable
                    if let Some(prev) = previous_last_slot {
                        if last_slot > prev {
                            let inc = format!(" (+{})", last_slot - prev);
                            display.push_str(&inc);
                        }
                    }
                }
                
                // Determine style
                let has_recent_increment = if let Some(prev) = previous_last_slot {
                    last_slot_info.map(|slot| slot > prev).unwrap_or(false)
                        && increment_time.map(|t| t.elapsed().as_secs() < 3).unwrap_or(false)
                } else {
                    false
                };
                
                let style = if has_recent_increment {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else if vote_data.is_voting {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                
                (display, style)
            };
            
            let (node_0_display, node_0_style) = build_vote_display(node_0.status == crate::types::NodeStatus::Active);
            let (node_1_display, node_1_style) = build_vote_display(node_1.status == crate::types::NodeStatus::Active);

            rows.push(Row::new(vec![
                Cell::from("Vote Status"),
                Cell::from(node_0_display).style(node_0_style),
                Cell::from(node_1_display).style(node_1_style),
            ]));
        } else {
            rows.push(Row::new(vec![
                Cell::from("Vote Status"),
                Cell::from("Loading..."),
                Cell::from("Loading..."),
            ]));
        }
    }

    // Add Alert Status row
    let alert_status = match &app_state.config.alert_config {
        Some(alert_config) if alert_config.enabled => {
            if alert_config.telegram.is_some() {
                "✅ Telegram"
            } else {
                "⚠️ Enabled (no method)"
            }
        }
        _ => "Disabled",
    };

    rows.push(Row::new(vec![
        Cell::from("Alert Status"),
        Cell::from(alert_status),
        Cell::from(alert_status),
    ]));

    let table = Table::new(
        rows,
        vec![
            Constraint::Length(20), // Wider label column for better spacing
            Constraint::Percentage(40),
            Constraint::Percentage(40),
        ],
    )
    .block(
        Block::default()
            .title(format!(
                "Identity: {} | Vote: {} | Time: {}",
                identity_formatted,
                vote_formatted,
                chrono::Local::now().format("%H:%M:%S")
            ))
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .padding(ratatui::widgets::Padding::new(1, 1, 0, 0)),
    );

    f.render_widget(table, padded_area);
}

// Removed draw_logs function as logs are no longer displayed

fn draw_footer(f: &mut ratatui::Frame, area: Rect, ui_state: &UiState) {
    // Check if any fields are refreshing
    let is_refreshing = ui_state.field_refresh_states.iter().any(|state| {
        state.node_0.status_refreshing || state.node_0.identity_refreshing || state.node_0.version_refreshing ||
        state.node_1.status_refreshing || state.node_1.identity_refreshing || state.node_1.version_refreshing
    });
    
    let refresh_indicator = if is_refreshing {
        " | 🔄 Refreshing..."
    } else {
        ""
    };
    
    let help_text = format!(
        "q/Esc: Quit | r: Refresh (5s) | s: Switch{}",
        refresh_indicator
    );

    let footer = Paragraph::new(help_text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);

    f.render_widget(footer, area);
}

/// Execute emergency failover for a validator
async fn execute_emergency_failover(
    validator_status: crate::ValidatorStatus,
    alert_manager: AlertManager,
    ssh_pool: Arc<crate::ssh::AsyncSshPool>,
    detected_ssh_keys: std::collections::HashMap<String, String>,
    emergency_takeover_flag: Arc<RwLock<bool>>,
) {
    // Find active and standby nodes
    let (active_node, standby_node) = match (
        validator_status.nodes_with_status.iter()
            .find(|n| n.status == crate::types::NodeStatus::Active),
        validator_status.nodes_with_status.iter()
            .find(|n| n.status == crate::types::NodeStatus::Standby),
    ) {
        (Some(active), Some(standby)) => (active.clone(), standby.clone()),
        _ => {
            eprintln!("❌ Emergency failover failed: could not identify active/standby nodes");
            return;
        }
    };

    // Set the emergency takeover flag to suspend UI rendering
    *emergency_takeover_flag.write().await = true;
    
    // Wait a moment for the UI to stop rendering and cleanup terminal
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    let mut emergency_failover = crate::emergency_failover::EmergencyFailover::new(
        active_node,
        standby_node,
        validator_status.validator_pair,
        ssh_pool,
        detected_ssh_keys,
        alert_manager,
    );

    if let Err(e) = emergency_failover.execute_emergency_takeover().await {
        eprintln!("❌ Emergency failover error: {}", e);
    }
    
    // Wait a moment for the user to see the results
    tokio::time::sleep(Duration::from_secs(3)).await;
    
    // Clear the emergency takeover flag to resume UI
    *emergency_takeover_flag.write().await = false;
}

/// Draw the switch UI
fn draw_switch_ui(f: &mut ratatui::Frame, app_state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Footer
        ])
        .split(f.size());

    // Header
    let header = Paragraph::new("🔄 SWITCH VALIDATOR")
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, chunks[0]);

    // Content area
    let content_chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints([
            Constraint::Length(10), // Status info
            Constraint::Length(10), // Actions
            Constraint::Min(0),     // Messages
        ])
        .split(chunks[1]);

    // Current status
    if !app_state.validator_statuses.is_empty() {
        let validator_status = &app_state.validator_statuses[0];

        let active_node = validator_status
            .nodes_with_status
            .iter()
            .find(|n| n.status == crate::types::NodeStatus::Active);
        let standby_node = validator_status
            .nodes_with_status
            .iter()
            .find(|n| n.status == crate::types::NodeStatus::Standby);

        let mut status_text = vec![];
        status_text.push(
            Line::from("Current State:").style(Style::default().add_modifier(Modifier::BOLD)),
        );

        if let (Some(active), Some(standby)) = (active_node, standby_node) {
            status_text.push(
                Line::from(format!("  {} → ACTIVE", active.node.label))
                    .style(Style::default().fg(Color::Green)),
            );
            status_text.push(
                Line::from(format!("  {} → STANDBY", standby.node.label))
                    .style(Style::default().fg(Color::Yellow)),
            );
            status_text.push(Line::from(""));
            status_text.push(
                Line::from("After Switch:").style(Style::default().add_modifier(Modifier::BOLD)),
            );
            status_text.push(
                Line::from(format!("  {} → STANDBY (was active)", active.node.label))
                    .style(Style::default().fg(Color::Yellow)),
            );
            status_text.push(
                Line::from(format!("  {} → ACTIVE (was standby)", standby.node.label))
                    .style(Style::default().fg(Color::Green)),
            );
        } else {
            status_text.push(
                Line::from("Unable to determine active/standby nodes")
                    .style(Style::default().fg(Color::Red)),
            );
        }

        let status_widget = Paragraph::new(status_text).block(
            Block::default()
                .title(" Status ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        f.render_widget(status_widget, content_chunks[0]);

        // Actions that will be performed
        let actions_text = vec![
            Line::from("Actions that will be performed:")
                .style(Style::default().add_modifier(Modifier::BOLD)),
            Line::from("  1. Switch active node to unfunded identity"),
            Line::from("  2. Transfer tower file to standby node"),
            Line::from("  3. Switch standby node to funded identity"),
            Line::from(""),
            Line::from("⚠️  Press 'y' to confirm switch or 'q' to cancel").style(
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
        ];

        let actions_widget = Paragraph::new(actions_text).block(
            Block::default()
                .title(" Switch Actions ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        );
        f.render_widget(actions_widget, content_chunks[1]);
    }

    // Footer
    let footer =
        Paragraph::new("Press 'y' to confirm switch | Press 'q' to cancel")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center);
    f.render_widget(footer, chunks[2]);
}

/// Helper function to shorten paths intelligently
fn shorten_path(path: &str, max_len: usize) -> String {
    if path == "N/A" || path.len() <= max_len {
        return path.to_string();
    }

    let parts: Vec<&str> = path.split('/').collect();

    // Always try to keep the filename intact
    if let Some(filename) = parts.last() {
        if filename.len() >= max_len - 3 {
            // If filename alone is too long, just truncate it
            return format!(
                "...{}",
                &filename[filename.len().saturating_sub(max_len - 3)..]
            );
        }

        // We have room for some path + filename
        let available = max_len - filename.len() - 4; // 4 for ".../filename"

        // Try to fit as much of the beginning path as possible
        let mut result = String::new();
        let mut used = 0;

        for (i, part) in parts[..parts.len() - 1].iter().enumerate() {
            if i == 0 && part.is_empty() {
                // Handle absolute paths
                continue;
            }

            let part_len = if i == 0 { part.len() + 1 } else { part.len() }; // +1 for leading /

            if used + part_len <= available {
                if i == 0 {
                    result.push('/');
                }
                result.push_str(part);
                if i < parts.len() - 2 {
                    result.push('/');
                }
                used += part_len + 1;
            } else if used == 0 && !part.is_empty() {
                // If we haven't added anything yet, at least add a shortened version of the first part
                let shortened = if part.len() > 4 { &part[..3] } else { part };
                result.push('/');
                result.push_str(shortened);
                result.push_str("...");
                break;
            } else {
                result.push_str("...");
                break;
            }
        }

        if result.is_empty() {
            result = "...".to_string();
        } else if !result.ends_with("...") && !result.ends_with('/') {
            result.push('/');
        }

        result.push_str(filename);
        result
    } else {
        path.to_string()
    }
}

/// Refresh all fields for all validators
async fn refresh_all_fields(app_state: Arc<AppState>, ui_state: Arc<RwLock<UiState>>) {
    // Get validator count from UI state
    let validator_count = {
        let ui_state_read = ui_state.read().await;
        ui_state_read.validator_statuses.len()
    };
    
    // Spawn refresh tasks for each validator
    let mut refresh_handles = Vec::new();
    for validator_idx in 0..validator_count {
        let app_state_clone = app_state.clone();
        let ui_state_clone = ui_state.clone();
        
        let handle = tokio::spawn(async move {
            refresh_validator_fields(validator_idx, app_state_clone, ui_state_clone).await;
        });
        refresh_handles.push(handle);
    }
    
    // Wait for all refreshes to complete
    for handle in refresh_handles {
        let _ = handle.await;
    }
    
    // Clear the global refreshing flag
    {
        let mut ui_state_write = ui_state.write().await;
        ui_state_write.is_refreshing = false;
    }
}

/// Refresh fields for a specific validator
async fn refresh_validator_fields(
    validator_idx: usize,
    app_state: Arc<AppState>,
    ui_state: Arc<RwLock<UiState>>,
) {
    // Get validator data from UI state
    let (validator_pair, nodes) = {
        let ui_state_read = ui_state.read().await;
        match ui_state_read.validator_statuses.get(validator_idx) {
            Some(v) => (v.validator_pair.clone(), v.nodes_with_status.clone()),
            None => return,
        }
    };
    
    // Refresh each node
    for (node_idx, node_with_status) in nodes.iter().enumerate() {
        let node = node_with_status.clone();
        let validator_pair_clone = validator_pair.clone();
        let ssh_pool = app_state.ssh_pool.clone();
        let ssh_key = app_state.detected_ssh_keys
            .get(&node.node.host)
            .cloned()
            .unwrap_or_default();
        
        // Refresh flags are already set in the key handler
        
        // Spawn refresh tasks for this node
        let ui_state_clone = ui_state.clone();
        let node_clone = node.clone();
        let ssh_pool_clone = ssh_pool.clone();
        let ssh_key_clone = ssh_key.clone();
        
        // Refresh status and identity
        tokio::spawn(async move {
            // Small delay to ensure UI shows loading state
            tokio::time::sleep(Duration::from_millis(50)).await;
            
            refresh_node_status_and_identity(
                validator_idx,
                node_idx,
                node_clone,
                validator_pair_clone.clone(),
                ssh_pool_clone,
                ssh_key_clone,
                ui_state_clone,
            ).await;
        });
        
        // Version refresh flag is already set in the key handler
        
        // Refresh version
        let ui_state_clone = ui_state.clone();
        let node_clone = node.clone();
        let ssh_pool_clone = ssh_pool.clone();
        let ssh_key_clone = ssh_key.clone();
        
        tokio::spawn(async move {
            // Small delay to ensure UI shows loading state
            tokio::time::sleep(Duration::from_millis(50)).await;
            
            refresh_node_version(
                validator_idx,
                node_idx,
                node_clone,
                ssh_pool_clone,
                ssh_key_clone,
                ui_state_clone,
            ).await;
        });
    }
}

/// Refresh node status and identity
async fn refresh_node_status_and_identity(
    validator_idx: usize,
    node_idx: usize,
    node: crate::types::NodeWithStatus,
    validator_pair: crate::types::ValidatorPair,
    ssh_pool: Arc<crate::ssh::AsyncSshPool>,
    ssh_key: String,
    ui_state: Arc<RwLock<UiState>>,
) {
    // Use the same logic as startup.rs to extract identity and status
    // First, get the solana CLI path
    let solana_cli = if let Some(ref cli) = node.solana_cli_executable {
        cli.clone()
    } else if node.validator_type == crate::types::ValidatorType::Firedancer {
        // For Firedancer, solana CLI is in the same directory as fdctl
        if let Some(ref fdctl_exec) = node.fdctl_executable {
            if let Some(fdctl_dir) = std::path::Path::new(fdctl_exec).parent() {
                fdctl_dir.join("solana").to_string_lossy().to_string()
            } else {
                "solana".to_string()
            }
        } else {
            "solana".to_string()
        }
    } else if let Some(ref agave_exec) = node.agave_validator_executable {
        agave_exec.replace("agave-validator", "solana")
    } else {
        // Try to find solana in common locations
        let check_cmd = "which solana || ls /home/solana/.local/share/solana/install/active_release/bin/solana 2>/dev/null || echo 'solana'";
        match ssh_pool.execute_command(&node.node, &ssh_key, check_cmd).await {
            Ok(output) => {
                let path = output.trim();
                if !path.is_empty() && path != "solana" {
                    path.to_string()
                } else {
                    // Fallback to default solana command
                    "solana".to_string()
                }
            }
            Err(_) => "solana".to_string()
        }
    };
    
    // Detect RPC port based on validator type
    let rpc_port = match node.validator_type {
        crate::types::ValidatorType::Firedancer => {
            // For Firedancer, get the config file and extract RPC port from TOML
            let mut port = 8899; // default
            
            // First, find the running fdctl process to get config path
            let ps_cmd = "ps aux | grep -E 'bin/fdctl' | grep -v grep";
            if let Ok(ps_output) = ssh_pool.execute_command(&node.node, &ssh_key, ps_cmd).await {
                // Extract config path from command line
                if let Some(line) = ps_output.lines().next() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    for (i, part) in parts.iter().enumerate() {
                        if part == &"--config" && i + 1 < parts.len() {
                            let config_path = parts[i + 1];
                            // Read RPC port from config
                            let grep_cmd = format!("cat {} | grep -A 5 '\\[rpc\\]' | grep 'port' | grep -o '[0-9]\\+' | head -1", config_path);
                            if let Ok(port_output) = ssh_pool.execute_command(&node.node, &ssh_key, &grep_cmd).await {
                                if let Ok(parsed_port) = port_output.trim().parse::<u16>() {
                                    port = parsed_port;
                                }
                            }
                            break;
                        }
                    }
                }
            }
            port
        }
        crate::types::ValidatorType::Agave | crate::types::ValidatorType::Jito => {
            // For Agave/Jito, extract --rpc-port from command line
            let mut port = 8899; // default
            
            let ps_cmd = "ps aux | grep -E 'agave-validator|solana-validator' | grep -v grep";
            if let Ok(ps_output) = ssh_pool.execute_command(&node.node, &ssh_key, ps_cmd).await {
                if let Some(line) = ps_output.lines().next() {
                    // Look for --rpc-port argument
                    if let Some(rpc_port_pos) = line.find("--rpc-port") {
                        let remaining = &line[rpc_port_pos + 10..]; // Skip "--rpc-port"
                        let parts: Vec<&str> = remaining.trim().split_whitespace().collect();
                        if !parts.is_empty() {
                            if let Ok(parsed_port) = parts[0].parse::<u16>() {
                                port = parsed_port;
                            }
                        }
                    }
                }
            }
            port
        }
        _ => 8899, // default for unknown types
    };
    
    // All validator types use RPC to get identity
    let rpc_command = format!(
        r#"curl -s http://localhost:{} -X POST -H "Content-Type: application/json" -d '{{"jsonrpc":"2.0","id":1,"method":"getIdentity"}}' 2>&1"#,
        rpc_port
    );
    let command = rpc_command;
    let use_rpc = true;
    
    
    let command_result = ssh_pool
        .execute_command(&node.node, &ssh_key, &command)
        .await;
    
    let (current_identity, _status, sync_status) = match command_result {
        Ok(output) => {
            
            let mut extracted_identity = None;
            let mut extracted_status = crate::types::NodeStatus::Unknown;
            let mut extracted_sync_status = None;
            
            if use_rpc {
                // Parse RPC response for Agave/Jito
                match serde_json::from_str::<serde_json::Value>(&output) {
                    Ok(json) => {
                        if let Some(identity) = json["result"]["identity"].as_str() {
                            extracted_identity = Some(identity.to_string());
                            
                            // Determine status based on identity match
                            if identity == validator_pair.identity_pubkey {
                                extracted_status = crate::types::NodeStatus::Active;
                            } else {
                                extracted_status = crate::types::NodeStatus::Standby;
                            }
                            
                            // For RPC, we need to run catchup separately to get sync status
                            // We'll do this after getting identity
                        }
                    }
                    Err(_e) => {
                        // Failed to parse RPC response
                    }
                }
            } else {
                // Parse catchup output to extract identity and sync status
                for line in output.lines() {
                    if line.contains(" has caught up") || line.contains("0 slot(s) behind") {
                    if let Some(caught_up_pos) = line.find(" has caught up") {
                        let identity = line[..caught_up_pos].trim();
                        if !identity.is_empty() {
                            extracted_identity = Some(identity.to_string());
                            
                            // Determine status based on identity match
                            if identity == validator_pair.identity_pubkey {
                                extracted_status = crate::types::NodeStatus::Active;
                            } else {
                                extracted_status = crate::types::NodeStatus::Standby;
                            }
                        }
                        
                        // Extract slot information
                        if let Some(us_start) = line.find("us:") {
                            let us_end = line[us_start + 3..]
                                .find(' ')
                                .unwrap_or(line.len() - us_start - 3)
                                + us_start
                                + 3;
                            let us_slot = &line[us_start + 3..us_end];
                            extracted_sync_status = Some(format!("Caught up (slot: {})", us_slot));
                        } else {
                            extracted_sync_status = Some("Caught up".to_string());
                        }
                        break;
                    } else if line.contains("0 slot(s) behind") {
                        // Extract slot information from Firedancer format
                        if let Some(us_start) = line.find("us:") {
                            let us_end = line[us_start + 3..]
                                .find(' ')
                                .unwrap_or(line.len() - us_start - 3)
                                + us_start
                                + 3;
                            let us_slot = &line[us_start + 3..us_end];
                            extracted_sync_status = Some(format!("Caught up (slot: {})", us_slot));
                        } else {
                            extracted_sync_status = Some("Caught up".to_string());
                        }
                    }
                }
                }
            }
            
            // If no sync status found, set to Unknown
            if extracted_sync_status.is_none() {
                extracted_sync_status = Some("Unknown".to_string());
            }
            
            (extracted_identity, extracted_status, extracted_sync_status)
        }
        Err(_e) => {
            (None, crate::types::NodeStatus::Unknown, Some("Unknown".to_string()))
        },
    };
    
    // If we got identity via RPC, now run catchup to get sync status
    let sync_status = if use_rpc && current_identity.is_some() {
        let catchup_command = format!("timeout 10 {} catchup --our-localhost 2>&1", solana_cli);
        
        match ssh_pool.execute_command(&node.node, &ssh_key, &catchup_command).await {
            Ok(output) => {
                let mut sync_status = None;
                
                for line in output.lines() {
                    if line.contains(" has caught up") || line.contains("0 slot(s) behind") {
                        // Extract slot information
                        if let Some(us_start) = line.find("us:") {
                            let us_end = line[us_start + 3..]
                                .find(' ')
                                .unwrap_or(line.len() - us_start - 3)
                                + us_start
                                + 3;
                            let us_slot = &line[us_start + 3..us_end];
                            sync_status = Some(format!("Caught up (slot: {})", us_slot));
                        } else {
                            sync_status = Some("Caught up".to_string());
                        }
                        break;
                    }
                }
                
                sync_status.or(Some("Unknown".to_string()))
            }
            Err(_e) => {
                Some("Unknown".to_string())
            }
        }
    } else {
        sync_status
    };
    
    // Update UI state with the new status and identity
    {
        let mut ui_state_write = ui_state.write().await;
        
        // Update the validator status in UI state
        if let Some(validator_status) = ui_state_write.validator_statuses.get_mut(validator_idx) {
            if let Some(node_with_status) = validator_status.nodes_with_status.get_mut(node_idx) {
                // Update status
                node_with_status.status = _status;
                
                // Update identity
                node_with_status.current_identity = current_identity;
                
                // Update sync status
                node_with_status.sync_status = sync_status;
            }
        }
        
        // Clear refreshing flags
        if let Some(refresh_state) = ui_state_write.field_refresh_states.get_mut(validator_idx) {
            let field_state = if node_idx == 0 { &mut refresh_state.node_0 } else { &mut refresh_state.node_1 };
            field_state.status_refreshing = false;
            field_state.identity_refreshing = false;
        }
    }
}

/// Refresh node version
async fn refresh_node_version(
    validator_idx: usize,
    node_idx: usize,
    node: crate::types::NodeWithStatus,
    ssh_pool: Arc<crate::ssh::AsyncSshPool>,
    ssh_key: String,
    ui_state: Arc<RwLock<UiState>>,
) {
    // Extract version based on validator type and using proper executable paths
    let (_validator_type, _version) = match node.validator_type {
        crate::types::ValidatorType::Firedancer => {
            if let Some(ref fdctl_exec) = node.fdctl_executable {
                let version_cmd = format!("timeout 10 {} version 2>/dev/null", fdctl_exec);
                let version_output = ssh_pool
                    .execute_command(&node.node, &ssh_key, &version_cmd)
                    .await
                    .unwrap_or_else(|_| "Unknown".to_string());
                
                // Parse fdctl version output - first part is version
                let version = if let Some(line) = version_output.lines().next() {
                    if let Some(version_match) = line.split_whitespace().next() {
                        Some(format!("Firedancer {}", version_match))
                    } else {
                        Some("Firedancer Unknown".to_string())
                    }
                } else {
                    Some("Firedancer Unknown".to_string())
                };
                
                (crate::types::ValidatorType::Firedancer, version)
            } else {
                (crate::types::ValidatorType::Firedancer, Some("Firedancer Unknown".to_string()))
            }
        }
        crate::types::ValidatorType::Agave | crate::types::ValidatorType::Jito => {
            if let Some(ref agave_exec) = node.agave_validator_executable {
                let version_cmd = format!("timeout 10 {} --version 2>/dev/null", agave_exec);
                let version_output = ssh_pool
                    .execute_command(&node.node, &ssh_key, &version_cmd)
                    .await
                    .unwrap_or_else(|_| "Unknown".to_string());
                
                // Parse version output
                let version = if let Some(line) = version_output.lines().next() {
                    if line.starts_with("agave-validator ") || line.starts_with("solana-cli ") {
                        // Extract version after the executable name
                        line.split_whitespace()
                            .nth(1)
                            .map(|v| v.to_string())
                    } else if line.contains("jito-") {
                        // Jito validator format
                        Some(line.trim().to_string())
                    } else {
                        Some(line.trim().to_string())
                    }
                } else {
                    None
                };
                
                // Determine if it's Jito based on version output
                let validator_type = if version.as_ref().map_or(false, |v| v.contains("jito")) {
                    crate::types::ValidatorType::Jito
                } else {
                    crate::types::ValidatorType::Agave
                };
                
                (validator_type, version)
            } else {
                (node.validator_type.clone(), None)
            }
        }
        crate::types::ValidatorType::Unknown => {
            // Try to detect validator type
            (crate::types::ValidatorType::Unknown, None)
        }
    };
    
    // Update UI state with the new version info
    {
        let mut ui_state_write = ui_state.write().await;
        
        // Update the validator status in UI state
        if let Some(validator_status) = ui_state_write.validator_statuses.get_mut(validator_idx) {
            if let Some(node_with_status) = validator_status.nodes_with_status.get_mut(node_idx) {
                // Update validator type and version
                node_with_status.validator_type = _validator_type;
                node_with_status.version = _version;
            }
        }
        
        // Clear refreshing flag
        if let Some(refresh_state) = ui_state_write.field_refresh_states.get_mut(validator_idx) {
            let field_state = if node_idx == 0 { &mut refresh_state.node_0 } else { &mut refresh_state.node_1 };
            field_state.version_refreshing = false;
        }
    }
}

/// Entry point for the enhanced UI
pub async fn show_enhanced_status_ui(app_state: &AppState) -> Result<()> {
    // Clear any startup output before starting the TUI
    print!("\x1B[2J\x1B[1;1H"); // Clear screen and move cursor to top
    std::io::stdout().flush()?;

    // Small delay to ensure all startup output is complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    let app_state_arc = Arc::new(app_state.clone());
    let mut app = EnhancedStatusApp::new(app_state_arc.clone()).await?;
    let switch_confirmed = run_enhanced_ui(&mut app).await?;
    
    if switch_confirmed {
        // Execute the switch
        // Use the switch command with confirmation already provided
        let mut app_state_mut = app_state.clone();
        let result = crate::commands::switch::switch_command_with_confirmation(
            false,  // not a dry run
            &mut app_state_mut,
            false,  // don't require confirmation again
        ).await?;
        
        if result {
            println!("\n✅ Switch completed successfully!");
        } else {
            println!("\n❌ Switch was not completed");
        }
    }
    
    Ok(())
}
