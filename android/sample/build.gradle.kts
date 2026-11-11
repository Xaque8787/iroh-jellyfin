plugins { id("com.android.application"); id("org.jetbrains.kotlin.android") }
android {
    namespace = "io.github.xaque8787.irohsample"
    compileSdk = 35
    defaultConfig { applicationId = "io.github.xaque8787.irohsample"; minSdk = 24; targetSdk = 35; versionCode = 1; versionName = project.version.toString() }
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
    kotlinOptions { jvmTarget = "17" }
}
dependencies {
    // Deliberately consume the standalone artifact, not a Gradle project dependency.
    implementation(files("../../dist/iroh-client-android-${project.version}.aar"))
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
}
