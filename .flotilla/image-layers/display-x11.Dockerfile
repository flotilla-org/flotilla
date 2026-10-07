# Ubuntu packages follow the selected base's apt sources; explicitly unpinned.
ARG BASE
FROM ${BASE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends xvfb xauth x11-utils libgl1-mesa-dri mesa-utils \
    && rm -rf /var/lib/apt/lists/*
ENV LIBGL_ALWAYS_SOFTWARE=1
COPY .flotilla/image-layers/50-display-x11.sh /etc/flotilla/prelude.d/50-display-x11.sh
