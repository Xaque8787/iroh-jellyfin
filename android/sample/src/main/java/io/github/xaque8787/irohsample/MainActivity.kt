package io.github.xaque8787.irohsample

import android.app.Activity
import android.os.Bundle
import android.net.ConnectivityManager
import android.net.Network
import android.widget.*
import io.github.xaque8787.iroh.*
import kotlinx.coroutines.*
import java.net.URL

/** Activity-owned demo. A playback application should instead keep the client in its
 * own playback owner/service and stop it when playback ends, not on every rotation. */
class MainActivity : Activity() {
    private val scope = MainScope()
    private var client: IrohClient? = null
    private lateinit var output: TextView
    private lateinit var connectivity: ConnectivityManager
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) { scope.launch { runCatching { client?.notifyNetworkChange() } } }
        override fun onLost(network: Network) { scope.launch { runCatching { client?.notifyNetworkChange() } } }
    }
    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        val layout = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val invitation = EditText(this).apply { hint = "Invitation link"; inputType = 0x81 }
        val path = EditText(this).apply { hint = "Relative HTTP path"; setText("System/Info/Public") }
        output = TextView(this)
        fun button(label: String, action: suspend () -> Unit) {
            layout.addView(Button(this).apply {
                text = label
                setOnClickListener { scope.launch {
                    try { action() } catch (e: IrohException) { output.text = "${e.code}: ${e.message}" }
                    catch (e: Exception) { output.text = e.javaClass.simpleName }
                } }
            })
        }
        layout.addView(invitation); layout.addView(path)
        button("Initialize") { if (client == null) client = IrohClient.initialize(applicationContext); output.text = "Initialized" }
        button("Pair") { requireNotNull(client).pair(invitation.text.toString()); invitation.text.clear(); output.text = "Paired" }
        button("Start proxy") { requireNotNull(client).startProxy(); output.text = "Proxy running on loopback" }
        button("Diagnostics") { val s = requireNotNull(client).getStatus(); output.text = "${s.state}; ${s.authorization}; path ${s.path}; RTT ${s.rttMs}; policy ${s.transportPolicy}" }
        button("HTTP request") {
            val url = requireNotNull(client).startProxy().proxyUrl
            val relative = path.text.toString()
            require(!relative.startsWith("/") && !relative.contains(":") && !relative.contains("..")) { "Use a relative path" }
            output.text = withContext(Dispatchers.IO) {
                val connection = URL(url + relative).openConnection().apply { connectTimeout = 15000; readTimeout = 15000 }
                connection.getInputStream().bufferedReader().use { reader -> val chars = CharArray(65536); val count = reader.read(chars); if (count < 0) "" else String(chars, 0, count) }
            }
        }
        button("Stop proxy") { client?.stopProxy(); output.text = "Stopped; pairing retained" }
        button("Forget pairing") { client?.forgetPairing(); output.text = "Local pairing removed; identity retained" }
        layout.addView(output); setContentView(layout)
        connectivity = getSystemService(ConnectivityManager::class.java)
        connectivity.registerDefaultNetworkCallback(networkCallback)
    }
    override fun onDestroy() {
        connectivity.unregisterNetworkCallback(networkCallback)
        scope.launch { withContext(NonCancellable) { client?.shutdown() }; scope.cancel() }
        super.onDestroy()
    }
}
