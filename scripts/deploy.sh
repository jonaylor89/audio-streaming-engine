#!/bin/bash

set -e

PROJECT_ID=${1:-"your-project-id"}
REGION=${2:-"us-central1"}
SERVICE_NAME="streaming-engine"

echo "🚀 Deploying Streaming Engine to Google Cloud Run"
echo "Project: $PROJECT_ID"
echo "Region: $REGION"
echo ""

BUCKET_NAME="${PROJECT_ID}-test-audio"

# Deploy to Cloud Run with scale-to-zero configuration
echo "🚀 Deploying to Cloud Run..."
gcloud run deploy $SERVICE_NAME \
  --source . \
  --platform managed \
  --region $REGION \
  --allow-unauthenticated \
  --memory 2Gi \
  --cpu 2 \
  --timeout 900 \
  --max-instances 10 \
  --min-instances 0 \
  --cpu-throttling \
  --execution-environment gen2 \
  --set-env-vars="APP_ENVIRONMENT=production,APP_STORAGE__CLIENT__GCS__BUCKET=$BUCKET_NAME" \
  --project=$PROJECT_ID

# Get the service URL
SERVICE_URL=$(gcloud run services describe $SERVICE_NAME --region=$REGION --format="value(status.url)" --project=$PROJECT_ID)

echo ""
echo "✅ Deployment complete!"
echo "🌐 Service URL: $SERVICE_URL"
echo ""
echo "Test your deployment:"
echo "curl $SERVICE_URL/health"
echo ""
echo "Use with MCP:"
echo "npx @streaming-engine/mcp-server --server=$SERVICE_URL"
echo ""
echo "🎛️ Monitor usage and costs:"
echo "https://console.cloud.google.com/run/detail/$REGION/$SERVICE_NAME/metrics?project=$PROJECT_ID"
