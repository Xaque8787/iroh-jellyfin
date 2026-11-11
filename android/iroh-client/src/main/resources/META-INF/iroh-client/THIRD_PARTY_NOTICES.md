The build embeds the JVM support classes from rustls-platform-verifier-android
0.2.0, upstream https://github.com/rustls/rustls-platform-verifier, licensed under
MIT OR Apache-2.0. The original prebuilt component is checksum verified by
scripts/android/prepare-verifier.py. Its manifest is not merged: the SDK does not
silently override application network security policy. An optional upstream CRL
allowlist template plus localhost is packaged as @xml/iroh_network_security_config.

Rust dependencies and their licenses are listed in Cargo.lock and their upstream
Cargo.toml metadata. Iroh and this SDK use MIT OR Apache-2.0. Kotlin and coroutines
use Apache-2.0. Applications retain responsibility for their distribution notices.
