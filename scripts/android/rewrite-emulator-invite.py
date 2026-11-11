"""Keep the existing invitation format; replace only its emulator-local address hint."""
import base64, json, sys
prefix = 'jellyfin-iroh://invite/'
link = sys.stdin.read().strip()
assert link.startswith(prefix)
payload = link[len(prefix):]
doc = json.loads(base64.urlsafe_b64decode(payload + '=' * (-len(payload) % 4)))
port = next(a['Ip'].rsplit(':', 1)[1] for a in doc['addr']['addrs'] if 'Ip' in a)
doc['addr']['addrs'] = [{'Ip': '10.0.2.2:' + port}]
print(prefix + base64.urlsafe_b64encode(json.dumps(doc).encode()).decode().rstrip('='))
