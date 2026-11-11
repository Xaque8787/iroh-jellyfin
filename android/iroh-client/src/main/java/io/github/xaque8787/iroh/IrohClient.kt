package io.github.xaque8787.iroh

import android.content.Context
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.withContext
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import org.json.JSONObject

/** Use a dedicated application-private directory. Exclude it from device/cloud backup. */
data class IrohConfig(
    val storageDirectory: File,
    val deviceName: String = "Android client",
    /** null: Iroh defaults; "none": direct LAN testing; otherwise custom HTTPS relay URL. */
    val relayUrl: String? = null,
)

data class ProxyConnection(val proxyUrl: String)
enum class ConnectionState { IDLE, CONNECTED, DISCONNECTED, CLOSED, UNKNOWN }
enum class AuthorizationStatus { AUTHORIZED, DENIED, REVOKED, UNKNOWN }
enum class TransportPath { DIRECT, RELAY, UNKNOWN }
enum class TransportPolicy { AUTO, P2P_ONLY, RELAY_ONLY, UNKNOWN }
enum class IrohErrorCode { INVALID_INVITATION, AUTHORIZATION_DENIED, NOT_PAIRED, CLOSED, CONFIGURATION, STORAGE, TRANSPORT, INTERNAL }
class IrohException(val code: IrohErrorCode, message: String, val serverCode: String? = null) : Exception(message)
data class ConnectionStatus(
    val state: ConnectionState,
    val authorization: AuthorizationStatus,
    val path: TransportPath,
    val rttMs: Double?,
    /** Configured host preference. The observed path is reported separately. */
    val transportPolicy: TransportPolicy?,
    val policyEnforcement: String?,
    val proxyUrl: String?,
    val lastError: String?,
)

internal object NativeBridge {
    init { System.loadLibrary("iroh_client") }
    external fun initializeTls(context: Context)
    external fun invoke(handle: Long, operation: String, argument: String): String
}

/** One live client per private storage directory. Explicitly call shutdown when finished.
 * Native work runs off the UI thread. Lifecycle mutations finish even if the calling
 * coroutine is cancelled; stopProxy/shutdown cancel active media streams and upgrades.
 */
class IrohClient private constructor(private var handle: Long) {
    private val mutex = Mutex()
    companion object {
        suspend fun initialize(context: Context, config: IrohConfig? = null): IrohClient {
            var created: IrohClient? = null
            try {
                return withContext(Dispatchers.IO) {
                    val effectiveConfig = config ?: IrohConfig(File(context.noBackupFilesDir, "iroh-client"))
                    val directory = effectiveConfig.storageDirectory.canonicalFile
                    val privateRoot = context.applicationInfo.dataDir.let(::File).canonicalFile
                    if (!directory.path.startsWith(privateRoot.path + File.separator)) throw IrohException(IrohErrorCode.CONFIGURATION, "Storage must be a dedicated app-private subdirectory")
                    NativeBridge.initializeTls(context.applicationContext)
                    val args = JSONObject().put("dataDir", directory.path).put("deviceName", effectiveConfig.deviceName).put("relayUrl", effectiveConfig.relayUrl)
                    IrohClient(decode(NativeBridge.invoke(0, "initialize", args.toString())).getLong("handle")).also { created = it }
                }
            } catch (error: Throwable) {
                withContext(NonCancellable) { created?.shutdown() }
                throw error
            }
        }
        private fun decode(reply: String): JSONObject {
            val result = JSONObject(reply)
            if (!result.getBoolean("ok")) {
                val code = enumValues<IrohErrorCode>().firstOrNull { it.name.equals(result.optString("code"), true) } ?: IrohErrorCode.INTERNAL
                throw IrohException(code, result.optString("message", "Native operation failed"), result.nullableString("serverCode"))
            }
            return result.getJSONObject("value")
        }
    }
    private suspend fun call(operation: String, argument: String = ""): JSONObject = withContext(Dispatchers.IO + NonCancellable) {
        mutex.withLock {
            if (handle == 0L) throw IrohException(IrohErrorCode.CLOSED, "Client is shut down")
            decode(NativeBridge.invoke(handle, operation, argument))
        }
    }
    suspend fun pair(invitationLink: String) { call("pair", invitationLink) }
    suspend fun startProxy(): ProxyConnection = ProxyConnection(call("startProxy").getString("proxy_url"))
    suspend fun stopProxy() { call("stopProxy") }
    /** Retains device identity. Does not revoke the device on the host. */
    suspend fun forgetPairing() { call("forgetPairing") }
    suspend fun notifyNetworkChange() { call("networkChange") }
    suspend fun getStatus(): ConnectionStatus {
        val s = call("status")
        return ConnectionStatus(
            enumValue(s.getString("state"), ConnectionState.UNKNOWN),
            enumValue(s.getString("authorization"), AuthorizationStatus.UNKNOWN),
            enumValue(s.getString("path"), TransportPath.UNKNOWN),
            if (s.isNull("rtt_ms")) null else s.getDouble("rtt_ms"),
            s.nullableString("transport_policy")?.let { enumValue(it, TransportPolicy.UNKNOWN) },
            s.nullableString("policy_enforcement"), s.nullableString("proxy_url"), s.nullableString("last_error"),
        )
    }
    suspend fun shutdown() = withContext(Dispatchers.IO + NonCancellable) {
        mutex.withLock {
            if (handle != 0L) {
                decode(NativeBridge.invoke(handle, "shutdown", ""))
                handle = 0
            }
        }
    }
}
private fun JSONObject.nullableString(key: String): String? = if (isNull(key)) null else optString(key).takeIf { it.isNotEmpty() }
private inline fun <reified T : Enum<T>> enumValue(value: String, fallback: T): T = enumValues<T>().firstOrNull { it.name.equals(value, true) } ?: fallback
