**This repo is AI generated and served for debugging. PLEASE DON'T TRUST IT.**

---

# Hyper Across Tokio Runtime Hang

This repository contains an example that demonstrates the potential issues that can occur when using Hyper-based HTTP clients (like reqwest) across multiple Tokio runtimes. This is a common issue encountered in production systems.

## The Problem

When a Hyper-based HTTP client (like `reqwest::Client`) is created in one Tokio runtime and then used within tasks running on a different runtime, it can lead to hangs, deadlocks, or other unexpected behavior. This is because:

1. The client may have background tasks tied to its creating runtime
2. Connection pools can cause cross-runtime contention
3. Hyper's internal operations may expect to be executed within the original runtime

The typical scenario where this manifests:

- A global Tokio runtime is created at application startup
- A shared HTTP client is initialized within this runtime
- Later, a task-specific runtime is created for isolation
- Tasks running on the second runtime use the HTTP client created in the global runtime
- Under certain conditions (especially under load), requests hang indefinitely

## Why Hangs Occur

The hang occurs due to several underlying reasons:

1. **Runtime-bound resources**: Hyper and reqwest clients have internal futures and tasks that are bound to their creating runtime.

2. **Connection pooling**: When a connection is taken from the pool in one runtime and returned in another, the pool management can deadlock.

3. **Task scheduling conflicts**: Tasks waiting for I/O completion might be scheduled on the wrong runtime.

4. **Lock contention**: Mutex/RwLock contention between runtimes can exacerbate the issue.

5. **Worker thread exhaustion**: If one runtime's worker threads are busy, they can't process tasks required by the client.

## How to Reproduce

The hang is difficult to reproduce in a simple example because:

1. It's often load-dependent and timing-sensitive
2. It happens more frequently with specific connection pool sizes and configurations
3. System resource limits might affect reproducibility

The code in this repo attempts to create conditions that might trigger the issue, though it may not hang on every system or every run.

## Solutions

There are several ways to solve this issue:

### 1. Single Runtime Architecture

The most straightforward solution is to use a single global Tokio runtime for your entire application:

```rust
// Create a single application-wide runtime
let runtime = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(num_cores * 2)
    .enable_all()
    .build()
    .unwrap();

// All async work happens on this runtime
runtime.block_on(async {
    // Initialize shared clients here
    let client = reqwest::Client::new();

    // All tasks use the same runtime
    tokio::spawn(async move {
        // Using client is safe here
    });
});
```

### 2. Client Per Runtime

If you need multiple runtimes, create a separate client for each runtime:

```rust
// Global runtime
let global_runtime = tokio::runtime::Runtime::new().unwrap();

// Task runtime
let task_runtime = tokio::runtime::Runtime::new().unwrap();

// Each runtime creates its own client
let global_client = global_runtime.block_on(async {
    reqwest::Client::new()
});

let task_client = task_runtime.block_on(async {
    reqwest::Client::new()
});
```

### 3. Runtime-Aware Client Factory

Create a factory that ensures clients are created on the runtime where they'll be used:

```rust
fn get_client_for_current_runtime() -> reqwest::Client {
    // Ensure we're on a Tokio runtime
    let _guard = tokio::runtime::Handle::current();

    // This client is tied to the current runtime
    reqwest::Client::new()
}
```

### 4. Client Proxy Pattern

Implement a proxy that marshals requests across runtime boundaries:

```rust
struct ClientProxy {
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
}

impl ClientProxy {
    fn new(runtime: &tokio::runtime::Runtime) -> Self {
        let handle = runtime.handle().clone();
        let client = runtime.block_on(async {
            reqwest::Client::new()
        });

        Self {
            client,
            runtime: handle,
        }
    }

    async fn get(&self, url: &str) -> Result<String, reqwest::Error> {
        let url = url.to_string();
        let client = self.client.clone();

        // Execute the request on the runtime that created the client
        self.runtime.spawn(async move {
            client.get(&url).send().await?.text().await
        }).await.unwrap()
    }
}
```

### 5. Channel-Based Request Forwarding

Use channels to forward requests to the runtime where the client was created:

```rust
struct RequestService {
    sender: mpsc::Sender<Request>,
}

struct Request {
    url: String,
    response_tx: oneshot::Sender<Result<String, reqwest::Error>>,
}

// In main runtime:
let (tx, mut rx) = mpsc::channel::<Request>(100);
let client = reqwest::Client::new();

tokio::spawn(async move {
    while let Some(req) = rx.recv().await {
        let resp = client.get(&req.url).send().await
            .and_then(|r| r.text());
        let _ = req.response_tx.send(resp);
    }
});

// From any runtime:
let (resp_tx, resp_rx) = oneshot::channel();
tx.send(Request {
    url: "https://example.com".to_string(),
    response_tx: resp_tx,
}).await?;

let response = resp_rx.await?;
```

## Best Practices

1. **Understand runtime boundaries**: Be aware of which runtime is executing each part of your code.

2. **Keep resources with their runtime**: Resources created on a runtime should generally be used on that same runtime.

3. **Use appropriate concurrency patterns**: When crossing runtime boundaries is necessary, use channels or other safe cross-runtime communication patterns.

4. **Monitor for hangs**: Add timeouts to detect and recover from potential hangs.

5. **Consider simpler architectures**: Often, a single runtime with appropriate task organization is simpler and more reliable than multiple runtimes.

## Conclusion

The issue of HTTP clients hanging when used across Tokio runtimes is primarily an architectural problem. By understanding the underlying causes and applying the appropriate patterns from above, you can build reliable systems that avoid these insidious hang conditions.
