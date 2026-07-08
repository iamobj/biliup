# syntax=docker/dockerfile:1.7

# Prepare biliup's source tree
FROM node:lts AS source
ARG repo_url=https://github.com/biliup/biliup
ARG branch_name=master

COPY . /biliup
RUN set -eux; \
	\
	if [ ! -f /biliup/biliup.spec ]; then \
	rm -rf /biliup; \
	git clone --depth 1 --branch "$branch_name" "$repo_url" /biliup; \
	fi;


# Build biliup's web-ui
FROM node:lts AS webui-builder

WORKDIR /biliup

COPY --from=source /biliup/package.json /biliup/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm \
	set -eux; \
	npm ci;

COPY --from=source /biliup ./
RUN set -eux; \
	npm run build;


# Build biliup's python wheel
FROM rust:latest AS wheel-builder

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
	--mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
	set -eux; \
	apt-get update; \
	apt-get install -y --no-install-recommends python3-pip g++ patchelf;

RUN --mount=type=cache,target=/root/.cache/pip \
	set -eux; \
	pip3 install maturin --break-system-packages;


WORKDIR /biliup

COPY --from=source /biliup ./
COPY --from=webui-builder /biliup/out /biliup/out

RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/usr/local/cargo/git \
	--mount=type=cache,target=/biliup/target \
	set -eux; \
	maturin build --release --out /tmp/wheels;


# Deploy Biliup
FROM python:3.13-slim AS biliup

ENV TZ="Asia/Shanghai"
ENV LANG="C.UTF-8"
ENV LANGUAGE="C.UTF-8"
ENV LC_ALL="C.UTF-8"
EXPOSE 19159/tcp
VOLUME /opt

# 需要遵守 wheel 文件名规范
COPY --from=wheel-builder /tmp/wheels/* /tmp/

RUN --mount=type=cache,target=/root/.cache/pip \
	set -eux; \
	\
	whl=$(ls /tmp/biliup*.whl); \
	pip3 install "$whl"; \
	# pip3 install "$whl[quickjs]"; \
	rm -rf /tmp/*;

RUN --mount=type=cache,target=/root/.cache/pip \
	set -eux; \
	\
	savedAptMark="$(apt-mark showmanual)"; \
	useApt=false; \
	apt-get update; \
	apt-get install -y --no-install-recommends \
		wget \
		curl \
		xz-utils \
		g++ \
	; \
	apt-mark auto '.*' > /dev/null; \
	apt-mark manual curl wget; \
	\
	arch="$(dpkg --print-architecture)"; arch="${arch##*-}"; \
	url='https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-'; \
	case "$arch" in \
		'amd64') \
			url="${url}linux64-gpl-8.1.tar.xz"; \
		;; \
		'arm64') \
			url="${url}linuxarm64-gpl-8.1.tar.xz"; \
		;; \
		*) \
			useApt=true; \
		;; \
	esac; \
	\
	if [ "$useApt" = true ] ; then \
		apt-get install -y --no-install-recommends \
			ffmpeg \
		; \
	else \
		wget -O ffmpeg.tar.xz "$url" --progress=dot:giga; \
		tar -xJf ffmpeg.tar.xz -C /usr/local --strip-components=1; \
		rm -rf \
			/usr/local/doc \
			/usr/local/man; \
		rm -rf \
			/usr/local/bin/ffprobe \
			/usr/local/bin/ffplay; \
		rm -rf \
			ffmpeg*; \
		chmod a+x /usr/local/* ; \
	fi; \
	\
	# 安装 quickjs 需要 g++
	pip3 install quickjs; \
	\
	# Clean up \
	[ -z "$savedAptMark" ] || apt-mark manual $savedAptMark; \
	apt-get purge -y --auto-remove -o APT::AutoRemove::RecommendsImportant=false; \
	rm -rf \
		/tmp/* \
		/usr/share/doc/* \
		/var/cache/* \
		/var/lib/apt/lists/* \
		/var/tmp/* \
		/var/log/* \
	;

WORKDIR /opt

ENTRYPOINT ["biliup"]
