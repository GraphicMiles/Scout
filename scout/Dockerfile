FROM node:22-alpine
ENV NODE_ENV=production PORT=8080
WORKDIR /app
COPY package.json ./
COPY src ./src
COPY sdk ./sdk
USER node
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s CMD node -e "fetch('http://127.0.0.1:'+process.env.PORT+'/health').then(r=>process.exit(r.ok?0:1),()=>process.exit(1))"
# Heap capped well under the 512 MB container limit; --expose-gc lets Scout trigger GC after large pages.
CMD ["node", "--max-old-space-size=192", "--expose-gc", "src/index.js"]
