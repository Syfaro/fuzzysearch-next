FROM ubuntu:22.04
EXPOSE 8080
WORKDIR /app
ENV METRICS_HOST=0.0.0.0:8080
RUN apt-get update && \
    apt-get install -y --no-install-recommends openssl ca-certificates ffmpeg && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*
COPY ./fuzzysearch-loader/fuzzysearch-loader /bin/fuzzysearch-loader
CMD ["/bin/fuzzysearch-loader"]
