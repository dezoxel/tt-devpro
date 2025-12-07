FROM gradle:8.5-jdk17 AS build
WORKDIR /app
COPY . .
RUN gradle build --no-daemon -x test

FROM eclipse-temurin:17-jre
WORKDIR /app
COPY --from=build /app/build/distributions/*.tar .
RUN tar -xf *.tar && rm *.tar && mv tt-devpro-* tt
ENTRYPOINT ["./tt/bin/tt-devpro"]
