FROM node:slim

COPY . /action
WORKDIR /action

RUN npm install --production

ENTRYPOINT ["npx", "danger", "ci"]
