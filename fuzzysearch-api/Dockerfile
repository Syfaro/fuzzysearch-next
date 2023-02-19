FROM ubuntu:22.04
EXPOSE 8080
WORKDIR /app
RUN apt-get update && \
    apt-get install -y openssl ca-certificates && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*
COPY ./fuzzysearch-api/dist ./dist
COPY ./fuzzysearch-api/fuzzysearch-api /bin/fuzzysearch-api
CMD ["/bin/fuzzysearch-api"]
