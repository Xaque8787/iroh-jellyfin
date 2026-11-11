"""Bundle the pinned upstream Android TLS support without exposing another AAR dependency."""
import hashlib, io, pathlib, tomllib, urllib.request, zipfile
root = pathlib.Path(__file__).resolve().parents[2]
lock = tomllib.loads((root / 'iroh/Cargo.lock').read_text())
version = next(p['version'] for p in lock['package'] if p['name'] == 'rustls-platform-verifier-android')
assert version == '0.2.0', 'Review/update the TLS JVM component when Cargo.lock changes'
commit = '1aa691352a5e0c215210dfc9a3c2f2078639dc91'
url = f'https://raw.githubusercontent.com/rustls/rustls-platform-verifier/{commit}/android-release-support/maven/org/rustls/rustls-platform-verifier/{version}/rustls-platform-verifier-{version}.aar'
data = urllib.request.urlopen(url, timeout=60).read()
assert hashlib.sha256(data).hexdigest() == 'aa021794230fbc2f0be355999e2cf67398dd563de066a2e17001ffbd0b69101b', 'TLS support checksum mismatch'
module = root / 'android/iroh-client'
with zipfile.ZipFile(io.BytesIO(data)) as aar:
    jar = module / 'libs/rustls-platform-verifier.jar'
    jar.parent.mkdir(parents=True, exist_ok=True)
    jar.write_bytes(aar.read('classes.jar'))
    # Optional app opt-in template. Do not silently replace the consumer's network rules.
    config = aar.read('res/xml/network_security_config.xml').decode()
    config = config.replace('</network-security-config>', '<domain-config cleartextTrafficPermitted="true"><domain>127.0.0.1</domain></domain-config>\n</network-security-config>')
    resource = module / 'src/main/res/xml/iroh_network_security_config.xml'
    resource.parent.mkdir(parents=True, exist_ok=True)
    resource.write_text(config)
print('Bundled verified rustls Android JVM support 0.2.0')
