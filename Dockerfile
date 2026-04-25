# Stage 1: Build static FFmpeg with required codecs
FROM debian:bookworm-slim AS ffmpeg-build

RUN echo "deb http://deb.debian.org/debian bookworm non-free" >> /etc/apt/sources.list && \
    apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    yasm \
    nasm \
    curl \
    ca-certificates \
    libz-dev \
    libbz2-dev \
    liblzma-dev \
    libmp3lame-dev \
    libvorbis-dev \
    libopus-dev \
    libfdk-aac-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /ffmpeg

# Download and build FFmpeg statically
ARG FFMPEG_VERSION=8.1
RUN curl -L https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.bz2 | tar xj --strip-components=1

RUN ./configure \
    --prefix=/opt/ffmpeg \
    --enable-static \
    --disable-shared \
    --disable-programs \
    --disable-doc \
    --disable-htmlpages \
    --disable-manpages \
    --disable-podpages \
    --disable-txtpages \
    --enable-pic \
    --enable-gpl \
    --enable-nonfree \
    --enable-libmp3lame \
    --enable-libvorbis \
    --enable-libopus \
    --enable-libfdk-aac \
    --disable-debug \
    --disable-ffplay \
    --disable-ffprobe \
    --disable-network \
    --disable-devices \
    --disable-filters \
    --enable-filter=abuffer \
    --enable-filter=abuffersink \
    --enable-filter=aformat \
    --enable-filter=anull \
    --enable-filter=atempo \
    --enable-filter=areverse \
    --enable-filter=volume \
    --enable-filter=loudnorm \
    --enable-filter=lowpass \
    --enable-filter=highpass \
    --enable-filter=bandpass \
    --enable-filter=bass \
    --enable-filter=treble \
    --enable-filter=aecho \
    --enable-filter=chorus \
    --enable-filter=flanger \
    --enable-filter=aphaser \
    --enable-filter=tremolo \
    --enable-filter=acompressor \
    --enable-filter=anlmdn \
    --enable-filter=afade \
    --enable-filter=acrossfade \
    --enable-filter=aresample \
    && make -j$(nproc) \
    && make install

# FFmpeg's `make install` already generates .pc files with correct
# Libs.private entries for static linking (fdk-aac, mp3lame, etc.),
# so no manual pkg-config generation is needed.

# -----------------------------

FROM lukemathwalker/cargo-chef:latest-rust-1.95.0 AS chef

WORKDIR /app

RUN echo "deb http://deb.debian.org/debian bookworm non-free" >> /etc/apt/sources.list && \
    apt-get update && apt-get install -y --no-install-recommends \
    lld \
    clang \
    libclang-dev \
    pkg-config \
    libz-dev \
    libbz2-dev \
    liblzma-dev \
    libmp3lame-dev \
    libvorbis-dev \
    libopus-dev \
    libfdk-aac-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy FFmpeg from build stage
COPY --from=ffmpeg-build /opt/ffmpeg /opt/ffmpeg

# Set environment for FFmpeg
ENV FFMPEG_DIR=/opt/ffmpeg
ENV FFMPEG_STATIC=1
ENV PKG_CONFIG_PATH=/opt/ffmpeg/lib/pkgconfig:$PKG_CONFIG_PATH

# -----------------------------

FROM chef AS planner

COPY . .

RUN cargo chef prepare --recipe-path recipe.json

# -----------------------------

FROM chef AS builder

COPY --from=planner /app/recipe.json recipe.json

# Build dependencies with FFmpeg static linking
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .

RUN cargo build --release

# ----------------------------

FROM debian:bookworm-slim AS runtime

WORKDIR /app

RUN echo "deb http://deb.debian.org/debian bookworm non-free" >> /etc/apt/sources.list && \
    apt-get update -y \
    && apt-get install -y --no-install-recommends \
        openssl ca-certificates curl \
        libmp3lame0 libvorbis0a libvorbisenc2 libopus0 libfdk-aac2 libogg0 \
    && apt-get autoremove -y \
    && apt-get clean -y \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/streaming-engine streaming-engine

COPY config config
ENV APP_ENVIRONMENT=production

# Cloud Run expects the app to listen on 0.0.0.0:$PORT
ENV APP_APPLICATION__HOST=0.0.0.0

# Health check for Cloud Run
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:${PORT:-8080}/health || exit 1

ENTRYPOINT ["./streaming-engine"]
