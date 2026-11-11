"""Preserve real host routes and add the emulator's host alias as a fallback."""
import base64
import ipaddress
import json
import sys

PREFIX = 'jellyfin-iroh://invite/'


def emulator_invitation(link):
    link = link.strip()
    if not link.startswith(PREFIX):
        raise ValueError('Not an Iroh invitation')
    payload = link[len(PREFIX):]
    doc = json.loads(base64.urlsafe_b64decode(payload + '=' * (-len(payload) % 4)))
    addresses = doc['addr']['addrs']
    # The emulator can reach the runner's LAN addresses through its virtual router.
    # Do not discard them: 10.0.2.2 is a special NAT alias, not the host's actual
    # interface address, and must not be the only route available on reconnect.
    for address in addresses:
        socket = address.get('Ip')
        if socket is None:
            continue
        host, port = socket.rsplit(':', 1)
        if ipaddress.ip_address(host.strip('[]')).version == 4:
            fallback = {'Ip': f'10.0.2.2:{port}'}
            if fallback not in addresses:
                addresses.append(fallback)
            break
    else:
        raise ValueError('Emulator fixture requires an advertised IPv4 host address')
    return PREFIX + base64.urlsafe_b64encode(json.dumps(doc).encode()).decode().rstrip('=')


if __name__ == '__main__':
    print(emulator_invitation(sys.stdin.read()))
