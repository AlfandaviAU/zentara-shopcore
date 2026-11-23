# The bank simulator from the legacy repo, unchanged (test infrastructure).
FROM python:3.13-slim
ENV PYTHONUNBUFFERED=1
RUN pip install --no-cache-dir flask==3.1.0 requests==2.32.3 pytest==8.3.4
WORKDIR /srv/bank
COPY bank_sim/app.py ./app.py
COPY smoke ./smoke
EXPOSE 5001
CMD ["python", "app.py"]
