FROM golang:1.25.2-bookworm AS go-driver

WORKDIR /driver
COPY go-driver/ ./
RUN go mod download \
    && CGO_ENABLED=0 go build -trimpath -o /mysql-go-driver-e2e .

FROM maven:3.9.11-eclipse-temurin-21 AS jdbc-driver

RUN mvn -q dependency:get -Dartifact=com.mysql:mysql-connector-j:9.6.0
COPY JdbcDriver.java /driver/JdbcDriver.java
RUN javac -cp /root/.m2/repository/com/mysql/mysql-connector-j/9.6.0/mysql-connector-j-9.6.0.jar \
    -d /driver /driver/JdbcDriver.java

FROM ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      mysql-client-8.0=8.0.46-0ubuntu0.24.04.4 \
      mysql-client-core-8.0=8.0.46-0ubuntu0.24.04.4 \
      openjdk-21-jre-headless \
    && rm -rf /var/lib/apt/lists/*

COPY --from=go-driver /mysql-go-driver-e2e /usr/local/bin/mysql-go-driver-e2e
COPY --from=jdbc-driver /driver/JdbcDriver.class /opt/mysql-drivers/JdbcDriver.class
COPY --from=jdbc-driver /root/.m2/repository/com/mysql/mysql-connector-j/9.6.0/mysql-connector-j-9.6.0.jar /opt/mysql-drivers/mysql-connector-j-9.6.0.jar
