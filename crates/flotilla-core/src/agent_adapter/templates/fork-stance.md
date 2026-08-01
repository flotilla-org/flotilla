{% block operating_instructions %}{{ super() }}Fork-stance constraints:
- Never add or repoint a git remote. Use the existing remote whose URL matches the delivery repository.
- Never open issues, pull requests, or comments against the upstream repository.
- Open the fork PR against the exact repository URL and target ref named in `## Work context`.
{% endblock %}
