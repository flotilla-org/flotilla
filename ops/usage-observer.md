---
kind: workflow_template
name: usage-observer
repos: [flotilla]
---
vessels:
  - name: observe
    crew:
      - role: poller
        needs: [host_account_reach]
        command: scripts/usage-observer
