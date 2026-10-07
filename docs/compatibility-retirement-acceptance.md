# Compatibility retirement acceptance

The shpool/tmux configuration readers and label rename aliases were retired after
the r591 roll. Before rolling a candidate without those readers, run the normal
live-store decoding gate on every fleet host:

```bash
/path/to/candidate/bin/flotilla --socket "$DAEMON_SOCKET" resource validate --from-daemon
```

That gate validates decoding, not metadata spelling. On each host, run this
read-only check with its running daemon to detect historical labels on Vessel
and TerminalSession resources. `jq -e` fails if either collection still contains
an underscored key, even when its canonical counterpart also exists:

```bash
set -euo pipefail
for kind in Vessel TerminalSession; do
  flotilla --socket "$DAEMON_SOCKET" resource list "$kind" --json |
    jq -e '
      .records | all(.[].object.metadata.labels // {};
        (has("flotilla.work/vessel_ref") or
         has("flotilla.work/vessel_ordinal") or
         has("flotilla.work/crew_ordinal")) | not)
    '
done
```

Run the check in every namespace containing fleet resources, adding the
`resource list --namespace` option when it differs from the default. If it fails,
inspect those records and correct their producing manifests or reconcile them
before rollout. A successful decode alone does not prove canonical selectors
will find them. Do not regenerate the golden corpus to hide a failure.

Check operator-authored preferences select available providers (`cleat` or
`passthrough` for terminal pools; `cmux` or `zellij` for presentation). Derived
configuration decoding preserves arbitrary backend strings; discovery records
an `UnknownProviderPreference` when no available provider matches, surfaced as
`unknown_provider_preference` with the configured key in discovery diagnostics.
It does not restore retired providers.

After activation, verify HTTP filtering for a known vessel reference on the
resource API using the host's usual authentication and TLS configuration:

```bash
curl --fail --get "$RESOURCE_API/apis/flotilla.work/v1/namespaces/flotilla/terminalsessions" \
  --data-urlencode "labelSelector=flotilla.work/vessel-ref=$VESSEL_REF"
curl --fail --get "$RESOURCE_API/apis/flotilla.work/v1/namespaces/flotilla/terminalsessions" \
  --data-urlencode 'includeReplicas=true' \
  --data-urlencode "labelSelector=flotilla.work/vessel-ref=$VESSEL_REF"
```

Both returned collections must contain only exact canonical matches. Refresh
stored corpora separately through the normal post-roll capture workflow.
