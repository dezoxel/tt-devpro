FROM gradle:8.5-jdk17 AS build
WORKDIR /app
COPY . .
RUN gradle build --no-daemon

FROM eclipse-temurin:17-jre
WORKDIR /app
COPY --from=build /app/build/libs/*.jar app.jar
COPY --from=build /app/build/distributions/*.tar .
RUN tar -xf *.tar && rm *.tar
ENTRYPOINT ["./tt-devpro-0.1.0/bin/tt-devpro"]
