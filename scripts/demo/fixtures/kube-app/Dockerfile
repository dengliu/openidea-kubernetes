FROM nginx:1.27-alpine

RUN printf '<!DOCTYPE html>\n<html>\n<head><title>simple-app</title></head>\n<body><h1>simple-app</h1><p>Kubernetes demo app</p></body>\n</html>\n' \
    > /usr/share/nginx/html/index.html

EXPOSE 80

USER daemon
COPY source dest
ARG name=defaultValue


CMD ["nginx", "-g", "daemon off;"]
