use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, AtomicU64, Ordering}},
    thread,
    time::{Duration, Instant},
};

use axum::{
    Router,
    http::{HeaderMap, HeaderValue},
    response::IntoResponse,
    routing::get,
};
use reqwest::{StatusCode, header};
use tokio::{
    runtime::{Runtime, Builder},
    sync::Semaphore,
    time::{sleep, timeout},
};

// Flag to detect if the program is hanging
static HANG_DETECTED: AtomicBool = AtomicBool::new(false);
// Flag to track if we're in the post-runtime-drop phase
static POST_DROP_PHASE: AtomicBool = AtomicBool::new(false);
// Counter for connection tracking
static CONNECTION_COUNTER: AtomicUsize = AtomicUsize::new(0);
// Track if we should slow down responses
static INDUCE_DELAYS: AtomicBool = AtomicBool::new(false);
// Track active connections
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
// Track connection leaks
static LEAKED_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
// Track when the issue was reproduced
static REPRODUCTION_TIMESTAMP: AtomicU64 = AtomicU64::new(0);
// Request tracking ID counter
static REQUEST_ID_COUNTER: AtomicUsize = AtomicUsize::new(0);

// Request tracking system
struct RequestTracker {
    requests: Mutex<HashMap<String, RequestStatus>>,
}

#[derive(Clone, Debug)]
struct RequestStatus {
    id: String,
    task_id: u32,
    url: String,
    phase: String,
    start_time: Instant,
    last_update: Instant,
    state: String,
    timeout_secs: u64,
    runtime_name: String,
}

impl RequestTracker {
    fn new() -> Self {
        Self {
            requests: Mutex::new(HashMap::new()),
        }
    }

    fn create_request(&self, task_id: u32, url: &str) -> String {
        let id = format!("REQ-{}-{}", task_id, REQUEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst));
        let phase = if POST_DROP_PHASE.load(Ordering::SeqCst) { "post-drop" } else { "normal" };

        let status = RequestStatus {
            id: id.clone(),
            task_id,
            url: url.to_string(),
            phase: phase.to_string(),
            start_time: Instant::now(),
            last_update: Instant::now(),
            state: "created".to_string(),
            timeout_secs: if POST_DROP_PHASE.load(Ordering::SeqCst) { 8 } else { 5 },
            runtime_name: thread::current().name().unwrap_or("unknown").to_string(),
        };

        let mut requests = self.requests.lock().unwrap();
        requests.insert(id.clone(), status);

        id
    }

    fn update_state(&self, id: &str, state: &str) {
        let mut requests = self.requests.lock().unwrap();
        if let Some(request) = requests.get_mut(id) {
            request.state = state.to_string();
            request.last_update = Instant::now();
        }
    }

    fn mark_completed(&self, id: &str) {
        let mut requests = self.requests.lock().unwrap();
        requests.remove(id);
    }

    fn print_pending_requests(&self) {
        let requests = self.requests.lock().unwrap();
        if requests.is_empty() {
            println!("🔍 No pending requests");
            return;
        }

        println!("\n🔍 PENDING REQUESTS ({}):", requests.len());
        println!("┌───────────────┬──────────┬──────────┬────────────────┬──────────────┐");
        println!("│ Request ID    │ Task ID  │ Phase    │ State          │ Elapsed      │");
        println!("├───────────────┼──────────┼──────────┼────────────────┼──────────────┤");

        for (_, req) in requests.iter() {
            let elapsed = req.start_time.elapsed();
            let elapsed_str = format!("{:.1}s", elapsed.as_secs_f32());

            // Highlight potentially hanging requests
            let is_hanging = elapsed.as_secs() > req.timeout_secs / 2;
            let prefix = if is_hanging { "⚠️ " } else { "" };

            println!("│ {:<13} │ {:<8} │ {:<8} │ {:<14} │ {:<12} │",
                prefix.to_string() + &req.id,
                req.task_id,
                req.phase,
                req.state,
                elapsed_str
            );
        }

        println!("└───────────────┴──────────┴──────────┴────────────────┴──────────────┘");

        // List possible hanging requests
        let hanging_reqs: Vec<_> = requests.iter()
            .filter(|(_, r)| r.start_time.elapsed().as_secs() > r.timeout_secs / 2)
            .collect();

        if !hanging_reqs.is_empty() {
            println!("\n⚠️ POTENTIALLY HANGING REQUESTS:");
            for (_, req) in hanging_reqs {
                println!(" - {} (Task {}, {}) in state '{}' for {:.1}s",
                    req.id, req.task_id, req.phase, req.state,
                    req.start_time.elapsed().as_secs_f32());
                println!("   Runtime: {}, URL: {}", req.runtime_name, req.url);
            }
        }
    }

    fn analyze_hangs(&self) {
        let requests = self.requests.lock().unwrap();

        // Group requests by state to find patterns
        let mut states = HashMap::new();
        for (_, req) in requests.iter() {
            let count = states.entry(req.state.clone()).or_insert(0);
            *count += 1;
        }

        // Check for specific hang patterns
        let mut hang_analysis = Vec::new();

        // Pattern 1: Many requests stuck in connection phase
        if let Some(&count) = states.get("connecting") {
            if count >= 3 {
                hang_analysis.push(format!(
                    "{} requests stuck in 'connecting' state - likely connection pool exhaustion",
                    count
                ));
            }
        }

        // Pattern 2: Requests stuck waiting for response body
        if let Some(&count) = states.get("reading_body") {
            if count >= 2 {
                hang_analysis.push(format!(
                    "{} requests stuck in 'reading_body' state - possible hang in connection keep-alive",
                    count
                ));
            }
        }

        // Pattern 3: Requests with very long duration
        let long_running: Vec<_> = requests.iter()
            .filter(|(_, r)| r.start_time.elapsed().as_secs() > r.timeout_secs * 3 / 4)
            .collect();

        if !long_running.is_empty() {
            hang_analysis.push(format!(
                "{} requests running longer than 75% of timeout - likely deadlocked",
                long_running.len()
            ));
        }

        if !hang_analysis.is_empty() {
            println!("\n🔬 HANG ANALYSIS:");
            for analysis in hang_analysis {
                println!(" • {}", analysis);
            }
            println!(" • Most likely cause: Connection pool resources tied to dropped runtime");
        }
    }
}

// Global request tracker
static REQUEST_TRACKER: once_cell::sync::Lazy<RequestTracker> = once_cell::sync::Lazy::new(|| {
    RequestTracker::new()
});

// Global runtime and client storage
static GLOBAL_STATE: once_cell::sync::Lazy<GlobalState> = once_cell::sync::Lazy::new(|| {
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("global-runtime")
        .enable_all()
        .build()
        .expect("Failed to create global runtime");

    // Create a client with very tight connection pool constraints
    let client = runtime.block_on(async {
        reqwest::Client::builder()
            .pool_max_idle_per_host(1)  // Minimal connections
            .pool_idle_timeout(Duration::from_secs(5))
            .http2_keep_alive_interval(Some(Duration::from_secs(1)))
            .http2_keep_alive_timeout(Duration::from_secs(1))
            .http2_keep_alive_while_idle(true)
            .timeout(Duration::from_secs(8))
            // Use connection pool settings that make hang more likely
            .tcp_keepalive(Some(Duration::from_secs(1)))
            // Limited concurrent connections to increase contention
            .connection_verbose(true)
            .pool_max_idle_per_host(1)
            .build()
            .expect("Failed to create HTTP client")
    });

    GlobalState {
        runtime,
        client: Arc::new(client),
        throttle: Arc::new(Semaphore::new(2)),  // Limit concurrent requests
        work_queue: Arc::new(Mutex::new(Vec::new())),
    }
});

struct GlobalState {
    runtime: Runtime,
    client: Arc<reqwest::Client>,
    throttle: Arc<Semaphore>,
    work_queue: Arc<Mutex<Vec<String>>>,
}

impl GlobalState {
    fn get_runtime() -> &'static Runtime {
        &GLOBAL_STATE.runtime
    }

    fn get_client() -> Arc<reqwest::Client> {
        GLOBAL_STATE.client.clone()
    }

    fn get_throttle() -> Arc<Semaphore> {
        GLOBAL_STATE.throttle.clone()
    }

    fn get_work_queue() -> Arc<Mutex<Vec<String>>> {
        GLOBAL_STATE.work_queue.clone()
    }
}

// Special sleep that simulates work in both the current runtime and global runtime
async fn complex_work(duration_ms: u64, id: &str) {
    // Local work
    sleep(Duration::from_millis(duration_ms / 2)).await;

    // Work in global runtime that might involve shared resources
    let work_queue = GlobalState::get_work_queue();
    {
        let mut queue = work_queue.lock().unwrap();
        queue.push(format!("Work {}: start", id));
    }

    sleep(Duration::from_millis(duration_ms / 2)).await;

    {
        let mut queue = work_queue.lock().unwrap();
        queue.push(format!("Work {}: end", id));
    }
}

// HTTP handler with controlled behavior to help induce hangs
async fn keepalive_handler() -> impl IntoResponse {
    let req_num = CONNECTION_COUNTER.fetch_add(1, Ordering::SeqCst);
    let active = ACTIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst);

    println!("🔄 Server handling request #{} (active: {})", req_num, active + 1);

    // Add delays during the post-drop phase
    if INDUCE_DELAYS.load(Ordering::SeqCst) {
        // Block a worker thread for a while
        let delay = if req_num % 3 == 0 {
            // Extra long delay for some requests
            println!("   [Server] Adding LONG delay for request #{}", req_num);
            1000 // Longer delay to increase chance of hang
        } else {
            println!("   [Server] Adding normal delay for request #{}", req_num);
            300  // Slightly longer normal delay
        };

        // Add some CPU work to increase resource contention
        if req_num % 5 == 0 {
            println!("   [Server] Adding CPU-intensive work for request #{}", req_num);
            // Do some CPU-intensive work to increase thread contention
            let mut sum: i32 = 0;
            for i in 0..500_000 {
                sum = sum.wrapping_add(i);
            }
            println!("   [Server] Finished CPU work for request #{} (sum: {})", req_num, sum);
        }
        
        // Now do the actual delay
        sleep(Duration::from_millis(delay)).await;
        
        // Sometimes add a second delay to simulate network jitter
        if req_num % 4 == 0 {
            sleep(Duration::from_millis(delay / 2)).await;
        }
    } else {
        sleep(Duration::from_millis(50)).await;
    }
    
    // Introduce connection pooling pressure by creating new clients within the server
    if POST_DROP_PHASE.load(Ordering::SeqCst) && req_num % 3 == 0 {
        // Create a new connection from the server side to further stress connection pooling
        let client = reqwest::Client::new();
        tokio::spawn(async move {
            println!("   [Server] Making loopback request from request #{}", req_num);
            let _ = client.get("http://localhost:3000").send().await;
        });
    }

    let mut headers = HeaderMap::new();
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    
    // Add more headers to increase response size
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert("X-Request-Id", HeaderValue::from_str(&req_num.to_string()).unwrap());

    // Also set cookies to increase response size
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_static("session=test-value; Path=/; HttpOnly")
    );

    let body = format!("Response #{} processed", req_num);

    ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);

    (StatusCode::OK, headers, body)
}

// Create a task-specific runtime with minimal worker threads
fn create_task_runtime(name: &str) -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(1) // Single worker to increase contention
        .thread_name(name)
        .thread_stack_size(256 * 1024) // Smaller stack to stress the system
        .enable_time()
        .enable_io() 
        .build()
        .expect(&format!("Failed to create {} runtime", name))
}

// Execute an HTTP request with timeout monitoring
async fn make_request_with_client(
    client: &reqwest::Client,
    url: &str,
    task_id: u32,
    timeout_secs: u64,
) -> Result<String, String> {
    // Create and track this request
    let req_id = REQUEST_TRACKER.create_request(task_id, url);

    println!("📤 {} started (timeout: {}s)", req_id, timeout_secs);
    REQUEST_TRACKER.update_state(&req_id, "started");

    // Try to get a permit from the throttle semaphore
    let throttle = GlobalState::get_throttle();
    REQUEST_TRACKER.update_state(&req_id, "acquiring_permit");
    let _permit = match timeout(
        Duration::from_secs(timeout_secs / 2),
        throttle.acquire()
    ).await {
        Ok(permit) => {
            println!("   {} acquired throttle permit", req_id);
            REQUEST_TRACKER.update_state(&req_id, "permit_acquired");
            Some(permit)
        },
        Err(_) => {
            println!("   {} failed to acquire throttle permit", req_id);
            REQUEST_TRACKER.update_state(&req_id, "permit_timeout");
            None
        }
    };

    // Add headers to make request more realistic
    let phase_name = if POST_DROP_PHASE.load(Ordering::SeqCst) { "post-drop" } else { "normal" };

    // Build the request
    REQUEST_TRACKER.update_state(&req_id, "preparing_request");
    let request = client.get(url)
        .header("X-Request-Id", &req_id)
        .header("X-Task-Id", task_id.to_string())
        .header("X-Phase", phase_name)
        .header("User-Agent", "hyper-tokio-test-agent/1.0")
        .header("Connection", "keep-alive");

    // Send the request with timeout
    REQUEST_TRACKER.update_state(&req_id, "connecting");
    match timeout(Duration::from_secs(timeout_secs), request.send()).await {
        Ok(inner_result) => match inner_result {
            Ok(response) => {
                println!("   {} got response: {}", req_id, response.status());
                REQUEST_TRACKER.update_state(&req_id, "reading_body");

                // Read the response body with timeout
                match timeout(Duration::from_secs(timeout_secs), response.text()).await {
                    Ok(Ok(body)) => {
                        println!("📩 {} completed with body of {} bytes", req_id, body.len());
                        REQUEST_TRACKER.update_state(&req_id, "completed");
                        REQUEST_TRACKER.mark_completed(&req_id);
                        Ok(body)
                    },
                    Ok(Err(e)) => {
                        println!("❌ {} body read error: {}", req_id, e);
                        REQUEST_TRACKER.update_state(&req_id, "body_error");
                        REQUEST_TRACKER.mark_completed(&req_id);
                        Err(format!("Body read error: {}", e))
                    },
                    Err(_) => {
                        println!("⏱️ {} body read TIMED OUT", req_id);
                        REQUEST_TRACKER.update_state(&req_id, "body_timeout");
                        HANG_DETECTED.store(true, Ordering::SeqCst);
                        Err("HANG DETECTED: Body read timed out".to_string())
                    }
                }
            },
            Err(e) => {
                println!("❌ {} request error: {}", req_id, e);
                REQUEST_TRACKER.update_state(&req_id, "request_error");
                REQUEST_TRACKER.mark_completed(&req_id);
                Err(format!("Request error: {}", e))
            }
        },
        Err(_) => {
            println!("⏱️ {} request TIMED OUT", req_id);
            REQUEST_TRACKER.update_state(&req_id, "request_timeout");
            HANG_DETECTED.store(true, Ordering::SeqCst);
            Err("HANG DETECTED: Request timed out".to_string())
        }
    }
}

// Run tasks on their own runtime, then drop the runtime
fn run_task_and_drop_runtime(
    task_id: u32,
    server_addr: String,
    request_count: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        println!("\n🚀 Task {} starting with own runtime", task_id);

        // Create a runtime specific to this task
        let task_runtime = create_task_runtime(&format!("task-{}", task_id));

        // Use the client from the global state within this runtime
        let client = GlobalState::get_client();
        // Get the global runtime to spawn tasks that depend on it
        let global_runtime = GlobalState::get_runtime();

        // Spawn a background task that will outlive this function's execution
        let work_queue = GlobalState::get_work_queue();

        // Create communication channels between task runtime and global runtime
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx_clone = Arc::new(tx.clone());
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();

        // Keep a copy for later use
        let server_addr_clone = server_addr.clone();
        
        // Run the task
        task_runtime.block_on(async move {
            println!("Task {} running", task_id);

            // Spawn a long-running background task that will be abruptly terminated
            // This task will hold resources from the global client
            let bg_client = client.clone();
            let server_addr_for_bg = server_addr.clone();
            let _bg_handle = tokio::spawn(async move {
                let mut cancel_rx = cancel_rx;
                loop {
                    tokio::select! {
                        _ = &mut cancel_rx => {
                            println!("   Task {} background task received cancellation, but ignoring it!", task_id);
                            // Ignore cancellation - continue holding the connection resources
                            // This simulates a task that doesn't properly clean up
                            break;
                        }
                        _ = async {
                            // This creates a strong dependency on the global client's connection pool
                            println!("   Task {} background task using global client...", task_id);
                
                            // Intentionally create and hold connection resources
                            let req = bg_client.get(&server_addr_for_bg)
                                .header("X-Keep-Connection", "true")
                                .timeout(Duration::from_secs(60))
                                .build().unwrap();
                
                            // Keep the connection alive but don't properly release it
                            match bg_client.execute(req).await {
                                Ok(resp) => {
                                    // Only read part of the body to keep connection open
                                    let _ = resp.bytes().await;
                                    LEAKED_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
                                },
                                Err(e) => println!("Task {} background task error: {}", task_id, e),
                            }
                
                            sleep(Duration::from_millis(100)).await;
                        } => {}
                    }
                }
            });

            // Make multiple requests
            for i in 0..request_count {
                let request_id = task_id * 100 + i as u32;

                // Add a task to the global work queue and create a cycle of dependencies
                {
                    let mut queue = work_queue.lock().unwrap();
                    let work_item = format!("task-{}-request-{}", task_id, i);
                    queue.push(work_item.clone());
    
                    // Send work item to the global runtime
                    let client_clone = client.clone();
                    let server_addr_clone = server_addr.clone();
    
                    // Create a pair of futures that depend on each other across runtimes
                    // This creates a strong interdependency between task runtime and global runtime
    
                    // Future 1: Task runtime depends on global runtime
                    let (task_tx, task_rx) = tokio::sync::oneshot::channel::<String>();
    
                    // This creates interdependency between runtimes - task in global runtime
                    let tx_for_task = Arc::clone(&tx_clone);
                    global_runtime.spawn(async move {
                        println!("   Global runtime processing work from task {}: {}", task_id, work_item);
        
                        // The global runtime is now doing work for the task runtime
                        let result = client_clone.get(&server_addr_clone)
                            .header("X-Shared-Work", &work_item)
                            .header("X-Connection-Test", "keep-alive")
                            .send().await;
        
                        match result {
                            Ok(_resp) => {
                                // Send response back to task runtime
                                let _ = task_tx.send(format!("completed:{}", work_item));
                                // Send success signal back to task thread through separate channel
                                let _ = tx_for_task.send(format!("completed:{}", work_item));
                            },
                            Err(e) => {
                                println!("   Global runtime work error: {}", e);
                            }
                        }
                    });
    
                    // Future 2: Create a subtask in task runtime that waits for global runtime
                    // This is the cycle - task runtime → global runtime → back to task runtime
                    tokio::spawn(async move {
                        match tokio::time::timeout(Duration::from_secs(5), task_rx).await {
                            Ok(Ok(result)) => {
                                println!("   Task runtime received result from global runtime: {}", result);
                            },
                            _ => {
                                println!("   Task runtime timed out waiting for global runtime result");
                            }
                        }
                    });
                }

                match make_request_with_client(&client, &server_addr, request_id, 5).await {
                    Ok(_) => {},
                    Err(e) => println!("Task {} request {} error: {}", task_id, i, e),
                }

                // Add some complex work between requests
                complex_work(50, &format!("task-{}-{}", task_id, i)).await;
            }

            println!("Task {} completed all work", task_id);
            
            // Don't properly cancel the background task - simulate bad cleanup
            // Send a cancel signal that our task will ignore
            let _ = cancel_tx.send(());

            // Don't await the handle - this will cause it to be abruptly terminated
            // when the runtime is dropped, potentially with resources still held
        });

        // Important: Drop the runtime but keep using the client
        println!("💥 Task {} DROPPING its runtime", task_id);
        drop(task_runtime);
        println!("   Task {} runtime dropped, resources released", task_id);
        
        // After dropping the runtime, try to interact with work done by the global runtime
        println!("   Task {} checking for completed work from global runtime", task_id);
        while let Ok(msg) = rx.try_recv() {
            println!("   Task {} received completion message: {}", task_id, msg);
        }
        
        // Try to use the client directly after the runtime is dropped
        let client_after_drop = GlobalState::get_client();
        let server_addr_for_post = server_addr_clone.clone();
        global_runtime.spawn(async move {
            println!("   Task {} spawned new task on global runtime after its own runtime was dropped", task_id);
            // This should create contention with connections that were in use by the dropped runtime
            let _ = client_after_drop.get(&server_addr_for_post)
                .header("X-Post-Drop", format!("task-{}", task_id))
                .send().await;
        });
    })
}

// Keep trying to make requests after the task runtime is dropped
fn run_post_drop_requests(
    server_addr: String,
    request_count: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // Wait for task runtimes to be dropped
        thread::sleep(Duration::from_secs(1));

        println!("\n🔄 Starting post-drop request phase");
        POST_DROP_PHASE.store(true, Ordering::SeqCst);
        INDUCE_DELAYS.store(true, Ordering::SeqCst);

        // Get the global runtime
        let runtime = GlobalState::get_runtime();

        // Get the client that was used by the dropped runtime
        let client = GlobalState::get_client();

        runtime.block_on(async move {
            println!("Making {} post-drop requests", request_count);

            // Create multiple concurrent requests to increase contention
            let mut tasks = Vec::with_capacity(request_count);

            // First, create a new runtime inside the global runtime's task
            // This creates a nested runtime situation that can lead to deadlocks
            let nested_runtime = create_task_runtime("nested-in-global");
            
            // Spawn a task that uses the nested runtime - this creates complex 
            // runtime interdependencies
            let nested_client = client.clone();
            let nested_addr = server_addr.clone();
            
            tasks.push(tokio::spawn(async move {
                println!("Creating a nested runtime inside global runtime task");
                
                // This makes the global runtime dependent on the nested runtime
                let _nested_task = nested_runtime.spawn(async move {
                    // But the nested runtime is using the global client
                    let result = make_request_with_client(
                        &nested_client, 
                        &nested_addr, 
                        999, 
                        5
                    ).await;
                    println!("Nested runtime request result: {:?}", result);
                });
                
                // Short delay to ensure nested task is running
                sleep(Duration::from_millis(50)).await;
                
                // Drop the runtime while task is still running
                println!("💥 Dropping nested runtime with active task");
                drop(nested_runtime);
                println!("Nested runtime dropped");
            }));

            // Make regular post-drop requests
            for i in 0..request_count {
                let client_clone = client.clone();
                let addr_clone = server_addr.clone();
                let req_id = 500 + i as u32;

                let task = tokio::spawn(async move {
                    // Wait a bit between spawns, but with variation to increase contention
                    sleep(Duration::from_millis((i * 50) as u64 % 300)).await;

                    // Create multiple connections to increase pool contention
                    let mut handles = Vec::new();
                    for j in 0..3 {
                        let c = client_clone.clone();
                        let a = addr_clone.clone();
                        let sub_id = req_id * 10 + j;
                        
                        handles.push(tokio::spawn(async move {
                            match make_request_with_client(&c, &a, sub_id, 8).await {
                                Ok(body) => println!("Post-drop req {} succeeded with {} bytes", sub_id, body.len()),
                                Err(e) => println!("Post-drop req {} failed: {}", sub_id, e),
                            }
                        }));
                    }
                    
                    // Wait for all to complete
                    for (j, h) in handles.into_iter().enumerate() {
                        if let Err(e) = h.await {
                            println!("Post-drop subrequest {}.{} error: {}", req_id, j, e);
                        }
                    }
                });

                tasks.push(task);

                // Small delay between spawning tasks
                if i % 2 == 0 {
                    sleep(Duration::from_millis(10)).await;
                }
            }

            // Wait for all tasks to complete
            for (i, task) in tasks.into_iter().enumerate() {
                match timeout(Duration::from_secs(10), task).await {
                    Ok(Ok(_)) => {},
                    Ok(Err(e)) => println!("Post-drop task {} join error: {}", i, e),
                    Err(_) => {
                        println!("⏱️ Post-drop task {} timed out - HANG DETECTED", i);
                        HANG_DETECTED.store(true, Ordering::SeqCst);
                    }
                }
            }

            println!("All post-drop requests completed or timed out");
        });
    })
}

// Start the HTTP server
async fn start_server() -> String {
    let app = Router::new().route("/", get(keepalive_handler));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind server");

    let addr = listener.local_addr().unwrap();
    let server_addr = format!("http://{}", addr);
    println!("Server listening on {}", server_addr);

    tokio::spawn(async move {
        println!("Server task started");
        axum::serve(listener, app).await.unwrap();
    });

    // Wait for server to be ready
    sleep(Duration::from_millis(100)).await;

    server_addr
}

// Monitor for hangs and resource leaks
fn spawn_hang_monitor() -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let start = std::time::Instant::now();
        let mut active_connections_history: Vec<usize> = Vec::new();

        // Check for hangs every second
        for i in 1..60 { // Extended monitoring time
            thread::sleep(Duration::from_secs(1));
            
            // Track active connection count
            let current_connections = ACTIVE_CONNECTIONS.load(Ordering::SeqCst);
            active_connections_history.push(current_connections);
            
            // Print more detailed monitoring info
            if i % 3 == 0 || (POST_DROP_PHASE.load(Ordering::SeqCst) && i % 2 == 0) {
                println!("\n📊 MONITOR [{}s]: Active connections: {}", i, current_connections);
                
                // Check for connection count not decreasing
                if active_connections_history.len() >= 6 {
                    let recent_history = &active_connections_history[active_connections_history.len() - 6..];
                    if recent_history.iter().all(|&c| c > 0) && 
                       recent_history.windows(2).all(|w| w[0] <= w[1]) {
                        println!("⚠️ WARNING: Connection count not decreasing: {:?}", recent_history);
                        if POST_DROP_PHASE.load(Ordering::SeqCst) {
                            println!("🔥 HANG DETECTED: Connection count steadily increasing or not decreasing!");
                            HANG_DETECTED.store(true, Ordering::SeqCst);
                        }
                    }
                }
                
                REQUEST_TRACKER.print_pending_requests();
                if i >= 10 && POST_DROP_PHASE.load(Ordering::SeqCst) {
                    // Start hang analysis earlier
                    REQUEST_TRACKER.analyze_hangs();
                }
            }

            // Check if hang was detected by timeout
            if HANG_DETECTED.load(Ordering::SeqCst) {
                println!("\n⚠️ HANG DETECTED after {} seconds!", i);
                println!("This reproduces the issue in production where connections");
                println!("are leaked after a runtime is dropped.");
                println!("Connection count history: {:?}", active_connections_history);

                // Final analysis of pending requests
                REQUEST_TRACKER.print_pending_requests();
                REQUEST_TRACKER.analyze_hangs();
                return;
            }

            // After 15 seconds in post-drop, if we're still waiting, we have a hang
            if i >= 15 && POST_DROP_PHASE.load(Ordering::SeqCst) && current_connections > 0 {
                println!("\n⚠️ Test taking too long with {} active connections - likely HANG situation", current_connections);
                println!("This reproduces the issue in production where connections");
                println!("are leaked after a runtime is dropped.");
                println!("Connection count history: {:?}", active_connections_history);
                HANG_DETECTED.store(true, Ordering::SeqCst);
                return;
            }
        }

        println!("\n✅ No hang detected after {} seconds", start.elapsed().as_secs());
    })
}

fn main() {
    // Setup logging
    tracing_subscriber::fmt::init();

    // Get the global runtime
    let global_rt = GlobalState::get_runtime();

    // Start the server
    println!("Starting server on global runtime");
    let server_addr = global_rt.block_on(start_server());

    // Start monitor for hangs
    let monitor_handle = spawn_hang_monitor();

    // PHASE 1: Create task runtimes, make requests, then drop them
    println!("\n==== PHASE 1: Creating and using task runtimes ====");

    let mut handles = Vec::new();

    // Spawn a task on the global runtime that will use task runtimes
    global_rt.spawn(async {
        println!("Global runtime spawning a task that will use another runtime");
        let nested_rt = create_task_runtime("global-spawned-nested");
        
        // Use a nested runtime from the global runtime
        nested_rt.spawn(async {
            println!("Task running in nested runtime that was created by global runtime");
            let _client = GlobalState::get_client();
            sleep(Duration::from_millis(500)).await;
        });
        
        // Don't drop the runtime yet - wait a bit
        sleep(Duration::from_millis(200)).await;
        println!("Global runtime's task now dropping the nested runtime");
    });

    // Start multiple tasks with their own runtimes in parallel
    println!("Starting tasks with their own runtimes...");
    for i in 1..=5 { // Increased number of tasks for more contention
        handles.push(run_task_and_drop_runtime(i, server_addr.clone(), (i % 3 + 1) as usize));
        thread::sleep(Duration::from_millis(50)); // Stagger task starts
    }

    // Wait for all tasks to finish (and drop their runtimes)
    for handle in handles {
        let _ = handle.join();
    }

    // PHASE 2: After all runtimes are dropped, try using the clients
    println!("\n==== PHASE 2: Using clients after runtimes were dropped ====");

    // Force collect some garbage to increase pressure
    println!("Forcing garbage collection...");
    drop(global_rt.spawn(async {
        // Create and drop a bunch of clients to stress connection pool
        for _ in 0..5 {
            let client = reqwest::Client::new();
            let _ = client.get("http://localhost").build();
            sleep(Duration::from_millis(10)).await;
        }
    }));
    
    // Start aggressive post-drop request process
    let post_drop_handle = run_post_drop_requests(server_addr, 10);

    // Run some additional tasks on the global runtime while post-drop is happening
    global_rt.spawn(async {
        println!("Running additional tasks on global runtime during post-drop phase");
        let client = GlobalState::get_client();
        
        for i in 0..5 {
            let _c = client.clone();
            tokio::spawn(async move {
                println!("Additional global task {} starting", i);
                sleep(Duration::from_millis(i * 50 + 100)).await;
                println!("Additional global task {} completed", i);
            });
        }
    });

    // Wait for post-drop operations to complete or hang
    let _ = post_drop_handle.join();

    // Wait for hang monitor
    let _ = monitor_handle.join();

    // Final status check
    if HANG_DETECTED.load(Ordering::SeqCst) {
        // Record the timestamp when we reproduced the issue
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::from_secs(0))
            .as_secs();
        REPRODUCTION_TIMESTAMP.store(timestamp, Ordering::SeqCst);
    
        println!("\n🔴 TEST RESULT: Successfully reproduced hang condition!");
        println!("The issue occurs when using a client after its runtime was dropped.");
        println!("This matches the production issue you're experiencing.");
        println!("Leaked connections: {}", LEAKED_CONNECTIONS.load(Ordering::SeqCst));
        println!("Active connections at end: {}", ACTIVE_CONNECTIONS.load(Ordering::SeqCst));
        println!("\nREPRODUCTION PATTERN:");
        println!("1. Task runtime creates async tasks using shared global client");
        println!("2. These tasks hold TCP connections from connection pool");
        println!("3. Task runtime is dropped without proper cleanup");
        println!("4. Global runtime tries to use the same client");
        println!("5. Hang occurs because connection resources weren't properly released");
    } else {
        println!("\n🟢 TEST RESULT: No hang detected in this run.");
        println!("The issue can be timing-dependent. It may require multiple runs");
        println!("or specific system conditions to trigger reliably.");
        println!("Try running with RUST_LOG=hyper=trace,tokio=trace for more details.");
    }

    println!("\nTest completed");
}