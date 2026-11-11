//! Private JNI adapter. The public Android API is the typed Kotlin wrapper.
use iroh_client_core::{Client, ClientConfig, Error};
use iroh_transport::net::RelayChoice;
use jni::{
    JNIEnv,
    objects::{JObject, JString},
    sys::{jlong, jstring},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicI64, Ordering},
    },
};
use tokio::runtime::Runtime;

struct NativeClient {
    runtime: Runtime,
    client: Client,
}
static CLIENTS: OnceLock<Mutex<HashMap<i64, Arc<NativeClient>>>> = OnceLock::new();
static NEXT: AtomicI64 = AtomicI64::new(1);
fn clients() -> &'static Mutex<HashMap<i64, Arc<NativeClient>>> {
    CLIENTS.get_or_init(Default::default)
}
fn failure(e: Error) -> Value {
    let server_code = match &e {
        Error::Denied(code) => Some(code.clone()),
        _ => None,
    };
    json!({"ok":false,"code":e.code(),"message":e.to_string(),"serverCode":server_code})
}
fn invoke(handle: i64, operation: &str, argument: &str) -> Result<Value, Error> {
    if operation == "initialize" {
        let config: Value = serde_json::from_str(argument)
            .map_err(|_| Error::Configuration("invalid configuration".into()))?;
        let path = config["dataDir"]
            .as_str()
            .ok_or_else(|| Error::Configuration("dataDir required".into()))?;
        if !std::path::Path::new(path).is_absolute() {
            return Err(Error::Configuration(
                "absolute private storage path required".into(),
            ));
        }
        let relay = match config["relayUrl"].as_str() {
            None | Some("") => RelayChoice::Default,
            Some("none") => RelayChoice::Disabled,
            Some(url) => RelayChoice::Custom(
                url.parse()
                    .map_err(|_| Error::Configuration("invalid relay URL".into()))?,
            ),
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| Error::Configuration(e.to_string()))?;
        let client = runtime.block_on(Client::new(ClientConfig {
            data_dir: path.into(),
            name: config["deviceName"]
                .as_str()
                .unwrap_or("Android client")
                .into(),
            relay,
        }))?;
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        clients()
            .lock()
            .map_err(|_| Error::Closed)?
            .insert(id, Arc::new(NativeClient { runtime, client }));
        return Ok(json!({"handle":id}));
    }
    let native = clients()
        .lock()
        .map_err(|_| Error::Closed)?
        .get(&handle)
        .cloned()
        .ok_or(Error::Closed)?;
    let result = native.runtime.block_on(async {
        match operation {
            "pair" => {
                native.client.pair(argument).await?;
                Ok(json!({}))
            }
            "startProxy" => Ok(serde_json::to_value(
                native
                    .client
                    .start_proxy("127.0.0.1:0".parse().expect("constant loopback"), true)
                    .await?,
            )
            .expect("serializable connection")),
            "status" => Ok(
                serde_json::to_value(native.client.status().await).expect("serializable status")
            ),
            "stopProxy" => {
                native.client.stop_proxy().await?;
                Ok(json!({}))
            }
            "forgetPairing" => {
                native.client.forget_pairing().await?;
                Ok(json!({}))
            }
            "networkChange" => {
                native.client.notify_network_change().await?;
                Ok(json!({}))
            }
            "shutdown" => {
                native.client.shutdown().await;
                Ok(json!({}))
            }
            _ => Err(Error::Configuration("unknown native operation".into())),
        }
    });
    if operation == "shutdown" {
        clients().lock().map_err(|_| Error::Closed)?.remove(&handle);
    }
    result
}
/// Panics never cross JNI. No raw pointers or pairing secrets are returned in errors/logs.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xaque8787_iroh_NativeBridge_invoke(
    mut env: JNIEnv,
    _object: JObject,
    handle: jlong,
    operation: JString,
    argument: JString,
) -> jstring {
    let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let operation: String = env
            .get_string(&operation)
            .map_err(|_| Error::Configuration("invalid operation encoding".into()))?
            .into();
        let argument: String = env
            .get_string(&argument)
            .map_err(|_| Error::Configuration("invalid argument encoding".into()))?
            .into();
        invoke(handle, &operation, &argument)
    }));
    let result = match reply {
        Ok(Ok(value)) => json!({"ok":true,"value":value}),
        Ok(Err(e)) => failure(e),
        Err(_) => json!({"ok":false,"code":"internal","message":"native operation failed"}),
    };
    match env.new_string(result.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_registry_owns_runtime_and_releases_storage_lock() {
        let dir = std::env::temp_dir().join(format!("iroh-jni-{}", std::process::id()));
        let config = json!({"dataDir":dir,"relayUrl":"none"}).to_string();
        let first = invoke(0, "initialize", &config).unwrap()["handle"]
            .as_i64()
            .unwrap();
        assert!(invoke(0, "initialize", &config).is_err());
        assert_eq!(invoke(first, "status", "").unwrap()["state"], "idle");
        assert!(matches!(
            invoke(first, "pair", "bad"),
            Err(Error::InvalidInvitation)
        ));
        assert!(matches!(
            invoke(first, "startProxy", ""),
            Err(Error::NotPaired)
        ));
        invoke(first, "networkChange", "").unwrap();
        invoke(first, "shutdown", "").unwrap();
        assert!(matches!(invoke(first, "status", ""), Err(Error::Closed)));
        let second = invoke(0, "initialize", &config).unwrap()["handle"]
            .as_i64()
            .unwrap();
        invoke(second, "shutdown", "").unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// reqwest/Iroh DNS HTTPS uses Android's platform certificate verifier. Initialize it
/// with the application context before creating any endpoint or background task.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_xaque8787_iroh_NativeBridge_initializeTls<'caller>(
    mut unowned_env: jni22::EnvUnowned<'caller>,
    _object: jni22::objects::JObject<'caller>,
    context: jni22::objects::JObject<'caller>,
) {
    use jni22::errors::ThrowRuntimeExAndDefault;
    unowned_env
        .with_env(|env| rustls_platform_verifier::android::init_with_env(env, context))
        .resolve::<ThrowRuntimeExAndDefault>();
}
