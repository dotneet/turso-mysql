FROM maven:3.9.16-eclipse-temurin-21@sha256:99e61abcff91a9b1333463bd8451fb18495d6eba9250ac66a338b518f8278320 AS build
WORKDIR /src
COPY pom.xml dependencies.lock.txt ./
COPY src ./src
# Maven has no lockfile: the build fails when the resolved dependency set
# differs from the committed list (regenerate it from target/dependencies.txt).
RUN --mount=type=cache,id=turso-e2e-m2,target=/root/.m2 \
    mvn -B -q package -DskipTests \
    && mvn -B -q dependency:list -Dsort=true -DoutputFile=/src/target/dependencies.txt \
    && diff -u dependencies.lock.txt target/dependencies.txt

FROM eclipse-temurin:21.0.12.1_1-jre@sha256:d7051a45dd955e4d5d1db4d3f4269fe13d1c6dff8cc6b7ef89fc8577b96c1982
WORKDIR /app
COPY --from=build /src/target/app.jar /src/target/dependencies.txt /app/
COPY run.sh /app/run.sh
RUN chmod +x /app/run.sh
