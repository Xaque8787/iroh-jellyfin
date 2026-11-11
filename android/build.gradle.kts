plugins {
    id("com.android.library") version "8.9.2" apply false
    id("com.android.application") version "8.9.2" apply false
    id("org.jetbrains.kotlin.android") version "2.1.20" apply false
}

// Keep the Android SDK and sample aligned with the host/CLI release version.
val releaseVersion = Regex("""(?m)^version = "([^"]+)"$""")
    .find(file("../iroh/Cargo.toml").readText())?.groupValues?.get(1)
    ?: error("Missing workspace release version")
allprojects { version = releaseVersion }
