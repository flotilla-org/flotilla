{% block operating_instructions %}{{ super() }}Fork-stance constraints:
- Never add a git remote. Provisioned checkouts intentionally contain only the fork remote (`origin`).
- Never open issues, pull requests, or comments against the upstream repository.
- Open the fork PR with its base set to the repository target named in the work context.
{% endblock %}
