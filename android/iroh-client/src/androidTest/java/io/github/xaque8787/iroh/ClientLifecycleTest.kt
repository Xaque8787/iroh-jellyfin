package io.github.xaque8787.iroh

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.net.HttpURLConnection
import java.net.URL

@RunWith(AndroidJUnit4::class)
class ClientLifecycleTest {
    private suspend fun <T> stage(name: String, action: suspend () -> T): T = try {
        action()
    } catch (error: Exception) {
        throw AssertionError("Android SDK stage failed: $name", error)
    }

    @Test fun nativeLifecycleAndStorage() = runBlocking {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val storage = File(context.noBackupFilesDir, "test-${System.nanoTime()}")
        val client = IrohClient.initialize(context, IrohConfig(storage, relayUrl = "none"))
        try {
            assertEquals(ConnectionState.IDLE, client.getStatus().state)
            try { client.pair("invalid"); fail("Expected invalid invitation") }
            catch (e: IrohException) { assertEquals(IrohErrorCode.INVALID_INVITATION, e.code) }
            try { client.startProxy(); fail("Expected not paired") }
            catch (e: IrohException) { assertEquals(IrohErrorCode.NOT_PAIRED, e.code) }
            client.notifyNetworkChange()
            client.stopProxy(); client.stopProxy(); client.forgetPairing()
            assertTrue(File(storage, "client.key").isFile)
        } finally { client.shutdown(); client.shutdown(); storage.deleteRecursively() }
    }
    /** Optional real host fixture provided by scripts/android/emulator-e2e.sh. */
    @Test fun realHostRangeAndReconnect() = runBlocking {
        val invite = InstrumentationRegistry.getArguments().getString("irohInvite")
        org.junit.Assume.assumeTrue("No real host fixture supplied", invite != null)
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val storage = File(context.noBackupFilesDir, "pair-${System.nanoTime()}")
        val client = IrohClient.initialize(context, IrohConfig(storage, relayUrl = "none"))
        try {
            stage("pair invitation") { client.pair(requireNotNull(invite)) }
            val connection = stage("start proxy after pairing") { client.startProxy() }
            assertEquals(connection, stage("reuse running proxy") { client.startProxy() })
            val http = URL(connection.proxyUrl + "movie.bin").openConnection() as HttpURLConnection
            http.setRequestProperty("Range", "bytes=10-19")
            http.connectTimeout = 15000; http.readTimeout = 15000
            try {
                assertEquals(206, http.responseCode)
                assertEquals(10, http.inputStream.use { it.readBytes().size })
            } finally { http.disconnect() }
            assertEquals(AuthorizationStatus.AUTHORIZED, client.getStatus().authorization)
            client.notifyNetworkChange()
            client.stopProxy()
            assertNotEquals(connection.proxyUrl, stage("restart proxy after stop") { client.startProxy() }.proxyUrl)
            client.forgetPairing()
        } finally { client.shutdown(); storage.deleteRecursively() }
    }
}
