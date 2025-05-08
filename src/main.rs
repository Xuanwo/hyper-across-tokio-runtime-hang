use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use axum::{
    routing::get,
    Router,
};
use once_cell::sync::Lazy;
use reqwest::Client;
use tokio::runtime::Runtime;
use tokio::sync::Semaphore;
use tokio::time::{sleep, timeout};

// Global flags for tracking the state
static HANG_DETECTED: AtomicBool = AtomicBool::new(false);
static POST_DROP_PHASE: AtomicBool = AtomicBool::new(false);
static REQUEST_ID_COUNTER: AtomicU32 = AtomicU32::new(1);

// Simple connection tracking to demonstrate the leak
#[derive(Debug)]
struct ConnectionInfo {
    id: u32,
    created_at: Instant,
    updated_at: Instant,
    url: String,
    runtime_name: String,
    state: String,
    task_id: u32,
}

// Global singleton for runtime and client management
static GLOBAL_STATE: Lazy<GlobalState> = Lazy::new(|| {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name_fn(|| {
            static ATOMIC_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
            format!("global-{}", id)
        })
        .build()
        .expect("Failed to create global runtime");

    // Create client with specific settings to make the leak more likely
    let client = Client::builder()
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(1) // Strict limit on connections per host
        .build()
        .expect("Failed to build reqwest client");

    GlobalState {
        runtime: Arc::new(runtime),
        client: Arc::new(client),
        throttle: Arc::new(Semaphore::new(10)),
        connections: Arc::new(Mutex::new(std::collections::HashMap::new())),
    }
});

struct GlobalState {
    runtime: Arc<Runtime>,
    client: Arc<Client>,
    throttle: Arc<Semaphore>,
    connections: Arc<Mutex<std::collections::HashMap<u32, ConnectionInfo>>>,
}

impl GlobalState {
    fn get_runtime() -> Arc<Runtime> {
        GLOBAL_STATE.runtime.clone()
    }

    fn get_client() -> Arc<Client> {
        GLOBAL_STATE.client.clone()
    }

    fn get_throttle() -> Arc<Semaphore> {
        GLOBAL_STATE.throttle.clone()
    }
    
    fn get_connections() -> Arc<Mutex<std::collections::HashMap<u32, ConnectionInfo>>> {
        GLOBAL_STATE.connections.clone()
    }
}

fn create_task_runtime(name: &str) -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name(name)
        .build()
        .expect("Failed to build task runtime")
}

async fn make_request_with_client(
    client: &reqwest::Client,
    url: &str,
    task_id: u32,
    timeout_secs: u64,
) -> Result<String, String> {
    let req_id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    println!("📤 Request {} started (timeout: {}s)", req_id, timeout_secs);

    // Get current runtime name for tracking
    let runtime_name = match std::thread::current().name() {
        Some(name) => name.to_string(),
        None => "unknown".to_string(),
    };

    // Record connection details
    let conn_id = req_id;
    let connections = GlobalState::get_connections();
    connections.lock().unwrap().insert(
        conn_id,
        ConnectionInfo {
            id: conn_id,
            created_at: Instant::now(),
            updated_at: Instant::now(),
            url: url.to_string(),
            runtime_name,
            state: "starting".to_string(),
            task_id,
        },
    );

    // Try to get a permit from the throttle semaphore
    let throttle = GlobalState::get_throttle();
    let _permit = match timeout(Duration::from_secs(timeout_secs / 2), throttle.acquire()).await {
        Ok(permit) => {
            println!("   {} acquired throttle permit", req_id);
            Some(permit)
        }
        Err(_) => {
            println!("   {} failed to acquire throttle permit", req_id);
            None
        }
    };

    // Get phase name
    let phase_name = if POST_DROP_PHASE.load(Ordering::SeqCst) {
        "post-drop"
    } else {
        "normal"
    };

    // Update connection state
    if let Some(conn) = connections.lock().unwrap().get_mut(&conn_id) {
        conn.state = "connecting".to_string();
    }

    // Build the request
    let request = client
        .get(url)
        .header("X-Request-Id", req_id.to_string())
        .header("X-Task-Id", task_id.to_string())
        .header("X-Phase", phase_name)
        .header("User-Agent", "hyper-tokio-test-agent/1.0")
        .header("Connection", "keep-alive");

    // Send the request with timeout
    match timeout(Duration::from_secs(timeout_secs), request.send()).await {
        Ok(inner_result) => match inner_result {
            Ok(response) => {
                println!("   {} got response: {}", req_id, response.status());
                
                if let Some(conn) = connections.lock().unwrap().get_mut(&conn_id) {
                    conn.state = "reading_body".to_string();
                }

                // Read the response body with timeout
                match timeout(Duration::from_secs(timeout_secs), response.text()).await {
                    Ok(Ok(body)) => {
                        println!("📩 {} completed with body of {} bytes", req_id, body.len());
                        
                        // Update connection state
                        if let Some(conn) = connections.lock().unwrap().get_mut(&conn_id) {
                            conn.state = "completed".to_string();
                            conn.updated_at = Instant::now();
                        }
                        
                        Ok(body)
                    }
                    Ok(Err(e)) => {
                        println!("❌ {} body read error: {}", req_id, e);
                        Err(format!("Body read error: {}", e))
                    }
                    Err(_) => {
                        println!("⏱️ {} body read TIMED OUT", req_id);
                        HANG_DETECTED.store(true, Ordering::SeqCst);
                        Err("HANG DETECTED: Body read timed out".to_string())
                    }
                }
            }
            Err(e) => {
                println!("❌ {} request error: {}", req_id, e);
                Err(format!("Request error: {}", e))
            }
        },
        Err(_) => {
            println!("⏱️ {} request TIMED OUT", req_id);
            HANG_DETECTED.store(true, Ordering::SeqCst);
            Err("HANG DETECTED: Request timed out".to_string())
        }
    }
}

// Run a task with its own runtime, then drop the runtime
fn run_task_with_runtime(task_id: u32, server_addr: String) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        println!("Task {} starting with own runtime", task_id);
        
        // Create a runtime for this task
        let runtime_name = format!("task-{}-rt", task_id);
        let rt = create_task_runtime(&runtime_name);
        
        // Get the shared client
        let client = GlobalState::get_client();
        
        // Run a request on this runtime
        rt.block_on(async {
            println!("Task {} making request on its runtime", task_id);
            let _ = make_request_with_client(&client, &server_addr, task_id, 5).await;
            
            // Small delay to ensure connection is established but still in use
            sleep(Duration::from_millis(500)).await;
            
            // Create a second connection from the same runtime
            let _ = make_request_with_client(&client, &server_addr, task_id + 100, 5).await;
        });
        
        // Runtime is implicitly dropped here which should cancel all active tasks
        println!("Task {} dropping its runtime", task_id);
    })
}

// This function creates the post-drop scenario that causes the hang
fn run_post_drop_requests(server_addr: String, request_count: usize) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // Wait for task runtimes to be dropped
        thread::sleep(Duration::from_secs(1));

        println!("\n🔄 Starting post-drop request phase");
        POST_DROP_PHASE.store(true, Ordering::SeqCst);

        // Get the global runtime
        let runtime = GlobalState::get_runtime();

        // Get the client that was used by the dropped runtime
        let client = GlobalState::get_client();

        runtime.block_on(async move {
            println!("Making {} post-drop requests", request_count);

            // Create multiple concurrent requests to increase contention
            let mut tasks = Vec::with_capacity(request_count * 3);
            
            // Make multiple post-drop requests
            for i in 0..request_count {
                let client_clone = client.clone();
                let addr_clone = server_addr.clone();
                let req_id = 500 + i as u32;

                let task = tokio::spawn(async move {
                    // Wait a bit between spawns with random variation
                    sleep(Duration::from_millis((i * 20) as u64 % 100)).await;

                    // Create multiple connections to increase pool contention
                    let mut handles = Vec::new();
                    for j in 0..3 {
                        let c = client_clone.clone();
                        let a = addr_clone.clone();
                        let sub_id = req_id * 10 + j;

                        handles.push(tokio::spawn(async move {
                            // Add delay to increase chance of observed problems
                            if j > 0 {
                                sleep(Duration::from_millis((j * 30) as u64)).await;
                            }
                            
                            match make_request_with_client(&c, &a, sub_id, 8).await {
                                Ok(body) => println!(
                                    "Post-drop req {} succeeded with {} bytes",
                                    sub_id,
                                    body.len()
                                ),
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
            }

            // Wait for all tasks to complete with a timeout
            for (i, task) in tasks.into_iter().enumerate() {
                match timeout(Duration::from_secs(8), task).await {
                    Ok(Ok(_)) => {}
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

// Server handler that adds controlled delays
async fn keepalive_handler() -> &'static str {
    // Simulate a slow response during post-drop phase
    if POST_DROP_PHASE.load(Ordering::SeqCst) {
        // Longer delay during post-drop phase to increase chance of hang
        sleep(Duration::from_millis(500)).await;
    } else {
        sleep(Duration::from_millis(50)).await;
    }
    
    "OK"
}

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

fn spawn_hang_monitor() -> thread::JoinHandle<()> {
    thread::spawn(|| {
        let interval_secs = 1;
        let max_wait_time = 30; // Maximum seconds to wait
        let mut elapsed = 0;
        let mut detected_long_running = false;
        
        while elapsed < max_wait_time {
            thread::sleep(Duration::from_secs(interval_secs));
            elapsed += interval_secs;
            
            if HANG_DETECTED.load(Ordering::SeqCst) {
                println!("🔄 Hang detected, analysis complete");
                return;
            }
            
            // Check if we're in post-drop phase and it's taking too long
            if POST_DROP_PHASE.load(Ordering::SeqCst) {
                if elapsed > 10 && !detected_long_running {
                    detected_long_running = true;
                    println!("⏱️ Post-drop phase running for over 10 seconds - potential hang forming");
                }
                
                if elapsed > 15 {
                    println!("⏱️ Post-drop phase taking too long - confirmed hang situation");
                    HANG_DETECTED.store(true, Ordering::SeqCst);
                    return;
                }
            }
        }
        
        // If we exit the loop without detecting a hang
        println!("⏰ Monitor timeout - test completed");
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

    // Start tasks with their own runtimes
    println!("Starting tasks with their own runtimes...");
    for i in 1..=3 {  // Create a few task runtimes
        handles.push(run_task_with_runtime(i, server_addr.clone()));
        thread::sleep(Duration::from_millis(20)); // Stagger task starts
    }

    // Wait for all tasks to finish (and drop their runtimes)
    for handle in handles {
        let _ = handle.join();
    }

    // PHASE 2: After all runtimes are dropped, try using the clients
    println!("\n==== PHASE 2: Using clients after runtimes were dropped ====");

    // Start post-drop request process with more concurrent requests
    let post_drop_handle = run_post_drop_requests(server_addr, 8);

    // Wait for post-drop operations to complete or hang
    let _ = post_drop_handle.join();

    // Wait for hang monitor
    let _ = monitor_handle.join();

    // Final status check
    if HANG_DETECTED.load(Ordering::SeqCst) {
        println!("\n🔴 TEST RESULT: Successfully reproduced hang condition!");
        println!("The issue occurs when using a client after its runtime was dropped.");
        println!("\n🔍 ROOT CAUSE ANALYSIS:");
        println!("1. Task runtime creates async tasks using shared global client");
        println!("2. These tasks hold TCP connections from connection pool");
        println!("3. Task runtime is dropped without proper cleanup");
        println!("4. Global runtime tries to use the same client");
        println!("5. Hang occurs because connection resources weren't properly released");
    } else {
        println!("\n🟢 TEST RESULT: No hang detected in this run.");
        println!("The issue can be timing-dependent. It may require multiple runs.");
    }

    println!("\nTest completed");
}