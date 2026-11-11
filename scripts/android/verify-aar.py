import io, sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as aar:
    expected = {f"jni/{abi}/libiroh_client.so" for abi in ("arm64-v8a", "armeabi-v7a", "x86_64")}
    assert {n for n in aar.namelist() if n.startswith("jni/") and n.endswith(".so")} == expected
    for abi in ("arm64-v8a", "armeabi-v7a", "x86_64"):
        assert len(aar.read(f"jni/{abi}/libiroh_client.so")) > 0
    assert aar.read("AndroidManifest.xml")
    with zipfile.ZipFile(io.BytesIO(aar.read("libs/rustls-platform-verifier.jar"))) as verifier:
        assert "org/rustls/platformverifier/CertificateVerifier.class" in verifier.namelist()
    with zipfile.ZipFile(io.BytesIO(aar.read("classes.jar"))) as classes:
        for name in ("IrohClient", "NativeBridge", "ConnectionStatus", "IrohConfig"):
            assert f"io/github/xaque8787/iroh/{name}.class" in classes.namelist()
print("AAR contains all native ABIs, manifest and Kotlin API")
