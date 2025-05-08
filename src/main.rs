use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use axum::{
    Router,
    http::{HeaderMap, HeaderValue},
    response::IntoResponse,
    routing::get,
};
use reqwest::{StatusCode, header};
use tokio::{
    runtime::{Builder, Handle, Runtime},
    sync::{RwLock, Semaphore, mpsc},
    time::{sleep, timeout},
};

// Flag to track if the program detected a hang
static HANG_DETECTED: AtomicBool = AtomicBool::new(false);
// Track which phase we're in
static POST_DROP_PHASE: AtomicBool = AtomicBool::new(false);
// Counter for connection tracking
static CONNECTION_COUNTER: AtomicUsize = AtomicUsize::new(0);
// Track if we should slow down responses
static INDUCE_DELAYS: AtomicBool = AtomicBool::new(false);
// Track active connections
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

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
            .pool_max_idle_per_host(1) // Minimal connections
            .pool_idle_timeout(Duration::from_secs(5))
            .http2_keep_alive_interval(Some(Duration::from_secs(1)))
            .http2_keep_alive_timeout(Duration::from_secs(1))
            .http2_keep_alive_while_idle(true)
            .pool_max_idle_per_host(2)
            .timeout(Duration::from_secs(8))
            .build()
            .expect("Failed to create HTTP client")
    });

    GlobalState {
        runtime,
        client: Arc::new(client),
        throttle: Arc::new(Semaphore::new(2)), // Limit concurrent requests
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

    println!(
        "🔄 Server handling request #{} (active: {})",
        req_num,
        active + 1
    );

    // Add delays during the post-drop phase
    if INDUCE_DELAYS.load(Ordering::SeqCst) {
        // Block a worker thread for a while
        let delay = if req_num % 3 == 0 {
            // Extra long delay for some requests
            println!("   [Server] Adding LONG delay for request #{}", req_num);
            500
        } else {
            println!("   [Server] Adding normal delay for request #{}", req_num);
            200
        };

        sleep(Duration::from_millis(delay)).await;
    } else {
        sleep(Duration::from_millis(50)).await;
    }

    let mut headers = HeaderMap::new();
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));

    // Also set cookies to increase response size
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_static("session=test-value; Path=/; HttpOnly"),
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
        .enable_all()
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
    let req_id = format!(
        "Req-{}-{}",
        task_id,
        CONNECTION_COUNTER.fetch_add(1, Ordering::SeqCst)
    );

    println!("📤 {} started (timeout: {}s)", req_id, timeout_secs);

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

    // Add headers to make request more realistic
    let phase_name = if POST_DROP_PHASE.load(Ordering::SeqCst) {
        "post-drop"
    } else {
        "normal"
    };

    match timeout(
        Duration::from_secs(timeout_secs),
        client
            .get(url)
            .header("X-Request-Id", &req_id)
            .header("X-Task-Id", task_id.to_string())
            .header("X-Phase", phase_name)
            .header("User-Agent", "hyper-tokio-test-agent/1.0")
            .header("Connection", "keep-alive")
            .send(),
    )
    .await
    {
        Ok(inner_result) => match inner_result {
            Ok(response) => {
                println!("   {} got response: {}", req_id, response.status());
                match timeout(Duration::from_secs(timeout_secs), response.text()).await {
                    Ok(Ok(body)) => {
                        println!("📩 {} completed with body of {} bytes", req_id, body.len());
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
                println!("❌ {} request error: {:?}", req_id, e);
                Err(format!("Request error: {:?}", e))
            }
        },
        Err(_) => {
            println!("⏱️ {} request TIMED OUT", req_id);
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

        // Run the task
        task_runtime.block_on(async move {
            println!("Task {} running", task_id);

            // Make multiple requests
            for i in 0..request_count {
                let request_id = task_id * 100 + i as u32;

                match make_request_with_client(&client, &server_addr, request_id, 5).await {
                    Ok(_) => {}
                    Err(e) => println!("Task {} request {} error: {}", task_id, i, e),
                }

                // Add some complex work between requests
                complex_work(50, &format!("task-{}-{}", task_id, i)).await;
            }

            println!("Task {} completed all work", task_id);
        });

        // Important: Drop the runtime but keep using the client
        println!("💥 Task {} DROPPING its runtime", task_id);
        drop(task_runtime);
        println!("   Task {} runtime dropped, resources released", task_id);
    })
}

// Keep trying to make requests after the task runtime is dropped
fn run_post_drop_requests(server_addr: String, request_count: usize) -> thread::JoinHandle<()> {
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

            for i in 0..request_count {
                let client_clone = client.clone();
                let addr_clone = server_addr.clone();
                let req_id = 500 + i as u32;

                let task = tokio::spawn(async move {
                    // Wait a bit between spawns, but with variation to increase contention
                    sleep(Duration::from_millis((i * 50) as u64 % 300)).await;

                    match make_request_with_client(&client_clone, &addr_clone, req_id, 8).await {
                        Ok(body) => println!(
                            "Post-drop req {} succeeded with {} bytes",
                            req_id,
                            body.len()
                        ),
                        Err(e) => println!("Post-drop req {} failed: {}", req_id, e),
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
                match task.await {
                    Ok(_) => {}
                    Err(e) => println!("Post-drop task {} join error: {}", i, e),
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

        // Check for hangs every second
        for i in 1..30 {
            thread::sleep(Duration::from_secs(1));

            // Check if hang was detected by timeout
            if HANG_DETECTED.load(Ordering::SeqCst) {
                println!("\n⚠️ HANG DETECTED after {} seconds!", i);
                println!("This reproduces the issue in production where connections");
                println!("are leaked after a runtime is dropped.");
                return;
            }

            // After 20 seconds, if we're still waiting, we have a hang
            if i >= 20 && POST_DROP_PHASE.load(Ordering::SeqCst) {
                println!("\n⚠️ Test taking too long - likely HANG situation");
                println!("This reproduces the issue in production where connections");
                println!("are leaked after a runtime is dropped.");
                HANG_DETECTED.store(true, Ordering::SeqCst);
                return;
            }
        }

        println!(
            "\n✅ No hang detected after {} seconds",
            start.elapsed().as_secs()
        );
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

    // Start 3 tasks with their own runtimes
    for i in 1..=3 {
        handles.push(run_task_and_drop_runtime(i, server_addr.clone(), 3));
        thread::sleep(Duration::from_millis(100)); // Stagger task starts
    }

    // Wait for all tasks to finish (and drop their runtimes)
    for handle in handles {
        let _ = handle.join();
    }

    // PHASE 2: After all runtimes are dropped, try using the clients
    println!("\n==== PHASE 2: Using clients after runtimes were dropped ====");

    // Start aggressive post-drop request process
    let post_drop_handle = run_post_drop_requests(server_addr, 10);

    // Wait for post-drop operations to complete or hang
    let _ = post_drop_handle.join();

    // Wait for hang monitor
    let _ = monitor_handle.join();

    // Final status check
    if HANG_DETECTED.load(Ordering::SeqCst) {
        println!("\n🔴 TEST RESULT: Successfully reproduced hang condition!");
        println!("The issue occurs when using a client after its runtime was dropped.");
        println!("This matches the production issue you're experiencing.");
    } else {
        println!("\n🟢 TEST RESULT: No hang detected in this run.");
        println!("The issue can be timing-dependent. It may require multiple runs");
        println!("or specific system conditions to trigger reliably.");
    }

    println!("\nTest completed");
}
