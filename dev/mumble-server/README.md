# Local Mumble server

A compose file for a local murmur instance to develop against.

```bash
podman compose -f dev/mumble-server/compose.yaml up -d
podman compose -f dev/mumble-server/compose.yaml logs -f
podman compose -f dev/mumble-server/compose.yaml down
```

Connect with server `localhost`, port `64738`, password `test`, and any username.
The SuperUser password is `superuser`.
