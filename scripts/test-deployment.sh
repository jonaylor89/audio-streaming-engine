#!/bin/bash

echo "🧪 Testing Streaming Engine Deployment Setup"
echo "======================================="

# Test Rust build
echo "📦 Testing Rust build..."
if cargo build --release; then
    echo "✅ Rust build successful"
else
    echo "❌ Rust build failed"
    exit 1
fi

# Test server startup (background)
echo "🚀 Testing server startup..."
timeout 10s cargo run &
SERVER_PID=$!
sleep 5

# Test health endpoint
echo "💚 Testing health endpoint..."
if curl -f http://localhost:8080/health >/dev/null 2>&1; then
    echo "✅ Health endpoint responding"
else
    echo "❌ Health endpoint not responding"
fi

# Test OpenAPI endpoint
echo "📋 Testing OpenAPI endpoint..."
if curl -f http://localhost:8080/api-schema >/dev/null 2>&1; then
    echo "✅ OpenAPI endpoint responding"
else
    echo "❌ OpenAPI endpoint not responding"
fi

# Stop server
kill $SERVER_PID 2>/dev/null

# Test MCP server package
echo "📱 Testing MCP server package..."
cd "$(dirname "$0")/../mcp-server"

# Test syntax of main files
if timeout 5s node --check index.js >/dev/null 2>&1; then
    echo "✅ MCP server index.js syntax valid"
else
    echo "❌ MCP server index.js syntax invalid"
fi

if timeout 5s node --check cli.js >/dev/null 2>&1; then
    echo "✅ MCP server cli.js syntax valid"
else
    echo "❌ MCP server cli.js syntax invalid"
fi

# Test package creation
if npm pack --dry-run >/dev/null 2>&1; then
    echo "✅ NPM package ready for publishing"
else
    echo "❌ NPM package has issues"
fi
cd ..

# Test Docker build (if Docker is available)
if command -v docker &> /dev/null; then
    echo "🐳 Testing Docker build..."
    if docker build -t streaming-engine-test . > /tmp/docker-build.log 2>&1; then
        echo "✅ Docker build successful"
        docker rmi streaming-engine-test >/dev/null 2>&1
    else
        echo "❌ Docker build failed"
        echo "🔍 Last 10 lines of build output:"
        tail -10 /tmp/docker-build.log
    fi
    rm -f /tmp/docker-build.log
else
    echo "⚠️  Docker not available, skipping Docker test"
fi

echo ""
echo "🎉 Deployment setup test complete!"
echo ""
echo "Next steps:"
echo "1. Publish MCP server: cd mcp-server && npm publish --access public"
echo "2. Deploy to Cloud Run: gcloud run deploy --source ."
echo "3. Test with: npx @streaming-engine/mcp-server --server=https://your-app.run.app"
