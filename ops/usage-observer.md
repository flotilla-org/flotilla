---
kind: workflow_template
name: usage-observer
repos: [flotilla]
---
vessels:
  - name: observe
    crew:
      - role: poller
        command: scripts/usage-observer
