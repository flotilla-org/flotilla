{% extends "builtin/crew.md" %}
{% block operating_instructions %}{{ super() }}This engagement is exactly one review and CI round. Finish, don't redo: for a GitHub destination, use the `pr-shepherd` skill to process the reviews, check results, conflicts, and fixes that are present when this engagement begins. For a Forgejo destination, do not use that GitHub-only helper; process the same round through the injected Forgejo API credentials. Future events belong to a later engagement.
{% endblock %}
{% block delivery %}Report what this round changed and the bound pull request's current readiness, then yield with `flotilla crew complete --message '<PR URL>'`. Do not merge the pull request. Run the completion command as your final act so the convoy returns to Landing.{% endblock %}
{% block assignment %}Finish, don't redo. Process this round's reviews and CI for the bound pull request using the `pr-shepherd` skill, report the result, and yield.{% endblock %}
